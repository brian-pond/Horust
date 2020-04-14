use crate::horust::bus::BusConnector;
use crate::horust::formats::{Event, ExitStatus, ServiceName, ServiceStatus};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub(crate) fn spawn(bus: BusConnector) {
    std::thread::spawn(move || {
        supervisor_thread(bus);
    });
}

struct Repo {
    // Contains services which are possibly started
    // but we don't know their pid yet ( ToBeRun / Initial)
    // Used to check if an exited process is a grandchild
    possibly_running: HashSet<ServiceName>,
    pids_map: HashMap<Pid, ServiceName>,
    bus: BusConnector,
    is_shutting_down: bool,
}

impl Repo {
    fn new(bus: BusConnector) -> Self {
        Repo {
            possibly_running: HashSet::new(),
            pids_map: HashMap::new(),
            bus,
            is_shutting_down: false,
        }
    }

    fn send_ev(&mut self, ev: Event) {
        self.bus.send_event(ev)
    }

    fn consume(&mut self, ev: Event) {
        match ev {
            Event::ShuttingDownInitiated => {
                self.is_shutting_down = true;
            }
            Event::PidChanged(service_name, pid) => {
                self.pids_map.insert(pid, service_name);
            }
            Event::StatusChanged(service_name, status) => {
                if vec![ServiceStatus::ToBeRun, ServiceStatus::Initial].contains(&status) {
                    self.possibly_running.insert(service_name);
                } else {
                    self.possibly_running.remove(&service_name);
                }
            }
            Event::ForceKill(service_name) => {
                self.possibly_running.remove(&service_name);
                // kill -9 won't trigger SIGCHILD.
                self.pids_map.retain(|_pid, sname| &service_name != sname);
            }
            _ => (),
        }
    }
    fn send_pid_exited(&mut self, pid: Pid, exit_code: i32) {
        let service_name = self.pids_map.remove(&pid).unwrap();
        debug!("Sending Service exited event: {}", service_name);
        self.bus
            .send_event(Event::new_service_exited(service_name, exit_code));
    }

    fn ingest(&mut self) {
        let updates: Vec<Event> = self.bus.try_get_events();
        updates.into_iter().for_each(|ev| self.consume(ev));
    }
}

/// A endlessly running function meant to be run in a separate thread.
/// Its purpose is to continuously try to reap possibly dead children.
pub(crate) fn supervisor_thread(bus: BusConnector) {
    let mut reapable = HashMap::new();
    let mut repo = Repo::new(bus);
    loop {
        repo.ingest();
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(wait_status) => {
                if let WaitStatus::Exited(pid, exit_code) = wait_status {
                    debug!("Pid has exited: {} with exitcode: {}", pid, exit_code);
                    reapable.insert(pid, exit_code);
                }
            }
            Err(err) => {
                if !err.to_string().contains("ECHILD") {
                    error!("Error waitpid(): {}", err);
                }
            }
        }
        // It might happen that before supervised was updated, the process was already started, executed,
        // and exited. Thus we're trying to reaping it, but there is still no map Pid -> Service.
        reapable.retain(|pid, exit_code| {
            if repo.pids_map.contains_key(pid) {
                repo.send_pid_exited(*pid, *exit_code);
                false
            } else {
                debug!("Pid exited was not in the map.");
                // If is a grandchildren, we don't care about it:
                // is grandchildren =
                !repo.possibly_running.is_empty()
            }
        });

        if repo.is_shutting_down && repo.possibly_running.is_empty() && repo.pids_map.is_empty() {
            debug!("Breaking the loop..");
            break;
        }
        std::thread::sleep(Duration::from_millis(300))
    }
    repo.send_ev(Event::Exiting("Reaper".into(), ExitStatus::Successful));
}
