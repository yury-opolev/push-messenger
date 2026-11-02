//! The supervisor with a fake spawner, fake children, a fake connector serving fake bridges
//! over `tokio::io::duplex`, a fake Scheduler, and paused time.

use std::collections::HashMap;
use std::sync::Mutex;

use push_bridge_control::{
    AgentInfo, BridgeState, MAX_LINE, Response, ServerLine, encode, parse_request,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

use super::*;
use crate::client::connect_stream;
use crate::schedtask::{ROOT_TASK_PATH, TaskInfo};

/// One fake bridge folder.
struct Bridge {
    listening: bool,
    state: BridgeState,
    /// Answer `shutdown` but keep running.
    ignore_shutdown: bool,
    /// The process the tray started for it.
    process: Option<Arc<Mutex<Option<Exit>>>>,
    /// Open connections: `Some(line)` to send, `None` to close.
    connections: Vec<mpsc::UnboundedSender<Option<String>>>,
    connects: usize,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            listening: false,
            state: BridgeState::Idle,
            ignore_shutdown: false,
            process: None,
            connections: Vec::new(),
            connects: 0,
        }
    }
}

#[derive(Default)]
struct World {
    bridges: HashMap<String, Bridge>,
    log: Vec<String>,
    /// A spawned bridge listens (else it exits with this code at once).
    exit_on_spawn: Option<i32>,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<World>>);

fn key(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut World) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }

    fn log(&self) -> Vec<String> {
        self.with(|world| world.log.clone())
    }

    fn count(&self, entry: &str) -> usize {
        self.log().iter().filter(|line| *line == entry).count()
    }

    fn listen(&self, name: &str) {
        self.with(|world| world.bridges.entry(name.into()).or_default().listening = true);
    }

    fn bridge<T>(&self, name: &str, f: impl FnOnce(&mut Bridge) -> T) -> T {
        self.with(|world| f(world.bridges.entry(name.into()).or_default()))
    }

    /// Sends an event on every connection to `name`.
    fn emit(&self, name: &str, event: Event) {
        self.bridge(name, |bridge| {
            if let Event::StateChanged { state, .. } = &event {
                bridge.state = *state;
            }
            let line = encode(&EventLine {
                event: event.clone(),
                at: 1,
            });
            for connection in &bridge.connections {
                let _ = connection.send(Some(line.clone()));
            }
        });
    }

    /// Closes every connection to `name`; it keeps listening.
    fn cut(&self, name: &str) {
        self.bridge(name, |bridge| {
            for connection in bridge.connections.drain(..) {
                let _ = connection.send(None);
            }
        });
    }
}

struct FakeChild {
    world: Shared,
    name: String,
    exit: Arc<Mutex<Option<Exit>>>,
}

impl Child for FakeChild {
    fn id(&self) -> Option<u32> {
        Some(7)
    }

    fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        Ok(*self.exit.lock().unwrap())
    }

    fn kill(&mut self) -> io::Result<()> {
        self.world
            .with(|world| world.log.push(format!("kill {}", self.name)));
        *self.exit.lock().unwrap() = Some(Exit { code: Some(1) });
        self.world
            .bridge(&self.name, |bridge| bridge.listening = false);
        self.world.cut(&self.name);
        Ok(())
    }
}

struct FakeSpawner(Shared);

impl Spawner for FakeSpawner {
    fn spawn(&self, dir: &Path) -> io::Result<Box<dyn Child>> {
        let name = key(dir);
        let exit = Arc::new(Mutex::new(None));
        self.0.with(|world| {
            world.log.push(format!("spawn {name}"));
            let code = world.exit_on_spawn;
            let bridge = world.bridges.entry(name.clone()).or_default();
            match code {
                Some(code) => *exit.lock().unwrap() = Some(Exit { code: Some(code) }),
                None => {
                    bridge.listening = true;
                    bridge.state = BridgeState::Idle;
                    bridge.process = Some(Arc::clone(&exit));
                }
            }
        });
        Ok(Box::new(FakeChild {
            world: self.0.clone(),
            name,
            exit,
        }))
    }
}

struct FakeConnector(Shared);

impl Connector for FakeConnector {
    fn connect<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, io::Result<Connection>> {
        let name = key(dir);
        let world = self.0.clone();
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(MAX_LINE * 2);
            let (sender, control) = mpsc::unbounded_channel();
            let state = world.bridge(&name, |bridge| {
                if !bridge.listening {
                    return None;
                }
                bridge.connects += 1;
                bridge.connections.push(sender);
                Some(bridge.state)
            });
            let Some(state) = state else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "nobody listens"));
            };
            tokio::spawn(serve(world, name, server, control, state));
            connect_stream(client).await
        })
    }
}

/// A fake bridge's side of one connection, like `push-bridge`'s ControlHub: hello, the
/// state, then responses and events.
async fn serve(
    world: Shared,
    name: String,
    stream: tokio::io::DuplexStream,
    mut control: mpsc::UnboundedReceiver<Option<String>>,
    state: BridgeState,
) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let hello = Event::Hello {
        protocol: 1,
        bridge: "0.1.0".into(),
        pid: 7,
        instance: instance_id(Path::new(&name)),
    };
    let mut first = encode(&EventLine {
        event: hello,
        at: 1,
    });
    first.push_str(&encode(&EventLine {
        event: Event::StateChanged {
            state,
            detail: None,
        },
        at: 1,
    }));
    if writer.write_all(first.as_bytes()).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { return };
                let request = parse_request(&line).unwrap();
                world.with(|w| w.log.push(format!("{} {name}", request.call.method())));
                let result = match request.call {
                    Call::Status => serde_json::to_value(Status {
                        state: Some(world.bridge(&name, |bridge| bridge.state)),
                        agent: Some(AgentInfo {
                            display_name: "Coda".into(),
                            handle: name.clone(),
                        }),
                        session: Some("s-1".into()),
                        ..Status::default()
                    })
                    .unwrap(),
                    _ => json!({}),
                };
                let response = encode(&ServerLine::Response(Response {
                    id: request.id,
                    ok: true,
                    result: Some(result),
                    error: None,
                }));
                if writer.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if request.call == Call::Shutdown {
                    let exits = world.bridge(&name, |bridge| {
                        if bridge.ignore_shutdown {
                            return false;
                        }
                        bridge.listening = false;
                        bridge.connections.clear();
                        if let Some(process) = &bridge.process {
                            *process.lock().unwrap() = Some(Exit { code: Some(0) });
                        }
                        true
                    });
                    if exits {
                        return;
                    }
                }
            }
            message = control.recv() => match message {
                Some(Some(line)) => {
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
                _ => return,
            },
        }
    }
}

struct FakeScheduler(Shared);

impl Scheduler for FakeScheduler {
    fn list(&self) -> io::Result<Vec<TaskInfo>> {
        Ok(Vec::new())
    }
    fn stop(&self, _: &TaskId) -> io::Result<()> {
        Ok(())
    }
    fn unregister(&self, _: &TaskId) -> io::Result<()> {
        Ok(())
    }
    /// The task starts the bridge named like the task (no process of the tray's).
    fn run(&self, task: &TaskId) -> io::Result<()> {
        self.0
            .with(|world| world.log.push(format!("run {}", task.name)));
        self.0.listen(&task.name);
        Ok(())
    }
}

struct Harness {
    commands: mpsc::Sender<Command>,
    views: watch::Receiver<Vec<InstanceView>>,
    notices: mpsc::Receiver<Notice>,
    run: JoinHandle<()>,
}

fn dir(name: &str) -> PathBuf {
    PathBuf::from(name)
}

fn spec(name: &str, autostart: bool) -> InstanceSpec {
    InstanceSpec {
        dir: dir(name),
        autostart,
        task: None,
    }
}

fn start(world: &Shared, specs: Vec<InstanceSpec>) -> Harness {
    let (supervisor, views, notices) = Supervisor::new(
        Arc::new(FakeSpawner(world.clone())),
        Arc::new(FakeConnector(world.clone())),
        Arc::new(FakeScheduler(world.clone())),
    );
    let (commands, receiver) = mpsc::channel(16);
    commands.try_send(Command::SetInstances(specs)).unwrap();
    Harness {
        commands,
        views,
        notices,
        run: tokio::spawn(supervisor.run(receiver)),
    }
}

impl Harness {
    async fn send(&self, command: Command) {
        self.commands.send(command).await.unwrap();
    }

    /// The views once `ready` holds for them (at most an hour of paused time).
    async fn until(
        &mut self,
        what: &str,
        ready: impl Fn(&[InstanceView]) -> bool,
    ) -> Vec<InstanceView> {
        let views = &mut self.views;
        let wait = async {
            loop {
                {
                    let current = views.borrow_and_update();
                    if ready(&current) {
                        return current.clone();
                    }
                }
                views.changed().await.expect("the supervisor is running");
            }
        };
        match tokio::time::timeout(Duration::from_secs(3600), wait).await {
            Ok(views) => views,
            Err(_) => panic!(
                "timed out waiting for {what}: {:?}",
                self.views
                    .borrow()
                    .iter()
                    .map(|v| &v.state)
                    .collect::<Vec<_>>()
            ),
        }
    }

    async fn state(&mut self, name: &str, state: AgentState) -> InstanceView {
        let views = self
            .until(&format!("{name} {state:?}"), |views| {
                views
                    .iter()
                    .any(|view| key(&view.dir) == name && view.state == state)
            })
            .await;
        views
            .into_iter()
            .find(|view| key(&view.dir) == name)
            .unwrap()
    }
}

fn labels(view: &InstanceView) -> Vec<String> {
    view.history
        .iter()
        .map(|(_, label)| label.clone())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn autostart_spawns_and_connects() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true), spec("b", false)]);
    let a = harness.state("a", AgentState::Idle).await;
    assert!(a.started_by_tray);
    assert!(a.connected);
    assert_eq!(world.log().first().map(String::as_str), Some("spawn a"));
    assert_eq!(world.count("spawn b"), 0);
    let a = harness
        .until("the status", |views| views[0].status.is_some())
        .await
        .remove(0);
    assert_eq!(a.name, "Coda (a)");
    assert_eq!(labels(&a), ["starting", "connected, idle"]);
    let b = harness.state("b", AgentState::Stopped).await;
    assert_eq!(b.name, "b");
    assert!(!b.started_by_tray);
}

#[tokio::test(start_paused = true)]
async fn a_reachable_bridge_is_external_and_never_spawned() {
    let world = Shared::default();
    world.listen("a");
    let mut harness = start(&world, vec![spec("a", true)]);
    let a = harness.state("a", AgentState::External).await;
    assert!(!a.started_by_tray);
    assert!(a.connected);
    harness.send(Command::Start(dir("a"))).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(world.count("spawn a"), 0);
}

#[tokio::test(start_paused = true)]
async fn five_exits_within_ten_minutes_give_up_with_a_notice() {
    let world = Shared::default();
    world.with(|world| world.exit_on_spawn = Some(1));
    let mut harness = start(&world, vec![spec("a", true)]);
    let a = harness
        .state("a", AgentState::GaveUp { code: Some(1) })
        .await;
    assert_eq!(world.count("spawn a"), 5);
    assert_eq!(a.last_exit, Some(1));
    assert!(labels(&a).contains(&"failed (exit 1), retrying in 1 s".to_owned()));
    assert!(labels(&a).contains(&"failed (exit 1), retrying in 8 s".to_owned()));
    assert_eq!(
        harness.notices.recv().await,
        Some(Notice::GaveUp {
            dir: dir("a"),
            name: "a".into(),
            code: Some(1),
        })
    );
    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    assert_eq!(world.count("spawn a"), 5);
}

#[tokio::test(start_paused = true)]
async fn exit_code_2_is_not_set_up_and_never_respawned() {
    let world = Shared::default();
    world.with(|world| world.exit_on_spawn = Some(2));
    let mut harness = start(&world, vec![spec("a", true)]);
    harness.state("a", AgentState::NotSetUp).await;
    assert_eq!(
        harness.notices.recv().await,
        Some(Notice::NotSetUp {
            dir: dir("a"),
            name: "a".into(),
        })
    );
    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    assert_eq!(world.count("spawn a"), 1);
}

#[tokio::test(start_paused = true)]
async fn stop_sends_shutdown_marks_stopped_and_does_not_restart() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true)]);
    harness.state("a", AgentState::Idle).await;
    harness.send(Command::Stop(dir("a"))).await;
    let a = harness.state("a", AgentState::Stopped).await;
    assert!(!a.started_by_tray);
    assert_eq!(a.last_exit, Some(0));
    assert_eq!(world.count("shutdown a"), 1);
    assert_eq!(world.count("kill a"), 0);
    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    assert_eq!(world.count("spawn a"), 1);
    assert_eq!(harness.views.borrow()[0].state, AgentState::Stopped);

    harness.send(Command::Start(dir("a"))).await;
    harness.state("a", AgentState::Idle).await;
    assert_eq!(world.count("spawn a"), 2);
}

#[tokio::test(start_paused = true)]
async fn a_bridge_that_does_not_exit_after_shutdown_is_killed() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true)]);
    harness.state("a", AgentState::Idle).await;
    world.bridge("a", |bridge| bridge.ignore_shutdown = true);
    let asked = Instant::now();
    harness.send(Command::Stop(dir("a"))).await;
    harness.state("a", AgentState::Stopping).await;
    harness.state("a", AgentState::Stopped).await;
    assert!(asked.elapsed() >= EXIT_GRACE);
    assert_eq!(world.count("shutdown a"), 1);
    assert_eq!(world.count("kill a"), 1);
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    assert_eq!(world.count("spawn a"), 1);
}

#[tokio::test(start_paused = true)]
async fn stopping_an_external_bridge_only_asks_it() {
    let world = Shared::default();
    world.listen("a");
    let mut harness = start(&world, vec![spec("a", false)]);
    harness.state("a", AgentState::External).await;
    harness.send(Command::Stop(dir("a"))).await;
    harness.state("a", AgentState::Stopped).await;
    assert_eq!(world.count("shutdown a"), 1);
    assert_eq!(world.count("spawn a"), 0);
    assert_eq!(world.count("kill a"), 0);
}

#[tokio::test(start_paused = true)]
async fn quit_stops_only_the_bridges_the_tray_started() {
    let world = Shared::default();
    world.listen("ext");
    let mut harness = start(&world, vec![spec("ext", true), spec("own", true)]);
    harness.state("ext", AgentState::External).await;
    harness.state("own", AgentState::Idle).await;
    harness.send(Command::Quit).await;
    tokio::time::timeout(Duration::from_secs(60), harness.run)
        .await
        .expect("run returns after Quit")
        .unwrap();
    assert_eq!(world.count("shutdown own"), 1);
    assert_eq!(world.count("shutdown ext"), 0);
    assert!(world.bridge("ext", |bridge| bridge.listening));
    assert!(!world.bridge("own", |bridge| bridge.listening));
    assert_eq!(world.count("kill own"), 0);
}

#[tokio::test(start_paused = true)]
async fn events_map_to_states() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true)]);
    harness.state("a", AgentState::Idle).await;
    world.emit(
        "a",
        Event::StateChanged {
            state: BridgeState::Waiting,
            detail: Some("approval".into()),
        },
    );
    let a = harness
        .state("a", AgentState::Waiting(Some("approval".into())))
        .await;
    assert_eq!(labels(&a).last().unwrap(), "waiting for you (approval)");
    world.emit(
        "a",
        Event::StateChanged {
            state: BridgeState::Paused,
            detail: None,
        },
    );
    harness.state("a", AgentState::Paused).await;
    world.emit(
        "a",
        Event::Error {
            code: "backend_crashed".into(),
            method: None,
        },
    );
    harness
        .until("the error", |views| {
            views[0].last_error.as_deref() == Some("backend_crashed")
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_to_a_live_bridge_reconnects() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true)]);
    harness.state("a", AgentState::Idle).await;
    world.cut("a");
    harness.state("a", AgentState::Starting).await;
    let a = harness.state("a", AgentState::Idle).await;
    assert!(a.connected);
    assert_eq!(world.bridge("a", |bridge| bridge.connects), 2);
    assert_eq!(world.count("spawn a"), 1);
    assert_eq!(
        labels(&a),
        ["starting", "connected, idle", "starting", "connected, idle"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_task_run_instance_is_started_through_its_task() {
    let world = Shared::default();
    let task = TaskId {
        path: ROOT_TASK_PATH.into(),
        name: "a".into(),
    };
    let mut harness = start(
        &world,
        vec![InstanceSpec {
            dir: dir("a"),
            autostart: true,
            task: Some(task.clone()),
        }],
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(harness.views.borrow()[0].state, AgentState::Stopped);
    assert_eq!(world.count("spawn a"), 0);
    harness.send(Command::Start(dir("a"))).await;
    let a = harness.state("a", AgentState::External).await;
    assert!(!a.started_by_tray);
    assert_eq!(a.task, Some(task));
    assert_eq!(world.count("run a"), 1);
    assert_eq!(world.count("spawn a"), 0);
}

#[tokio::test(start_paused = true)]
async fn calls_are_forwarded_and_failures_kept() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true), spec("b", false)]);
    harness.state("a", AgentState::Idle).await;
    harness.send(Command::RestartBackend(dir("a"))).await;
    harness.send(Command::Pause(dir("a"))).await;
    harness.send(Command::Resume(dir("a"))).await;
    harness.send(Command::Reconnect(dir("a"))).await;
    harness.send(Command::Reconnect(dir("b"))).await;
    harness
        .until("b's error", |views| {
            views[1].last_error.as_deref() == Some("reconnect: not connected")
        })
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for call in [
        "restart_backend",
        "pause_backend",
        "resume_backend",
        "reconnect",
    ] {
        assert_eq!(world.count(&format!("{call} a")), 1, "{call}");
    }
}

#[tokio::test(start_paused = true)]
async fn removed_instances_are_stopped_and_dropped() {
    let world = Shared::default();
    let mut harness = start(&world, vec![spec("a", true), spec("b", false)]);
    harness.state("a", AgentState::Idle).await;
    harness
        .send(Command::SetInstances(vec![spec("b", false)]))
        .await;
    harness
        .until("a removed", |views| {
            views.len() == 1 && key(&views[0].dir) == "b"
        })
        .await;
    assert_eq!(world.count("shutdown a"), 1);
}

#[test]
fn specs_carry_the_task_of_each_instance() {
    let config = TrayConfig {
        instances: vec![
            crate::tray_config::Instance {
                dir: dir("a"),
                autostart: true,
            },
            crate::tray_config::Instance {
                dir: dir("b"),
                autostart: false,
            },
        ],
        ..TrayConfig::default()
    };
    let found = [TaskFor {
        task: "t".into(),
        path: r"\Push\".into(),
        dir: dir("b"),
    }];
    assert_eq!(
        specs(&config, &found),
        [
            spec("a", true),
            InstanceSpec {
                dir: dir("b"),
                autostart: false,
                task: Some(TaskId {
                    path: r"\Push\".into(),
                    name: "t".into(),
                }),
            },
        ]
    );
}

#[test]
fn the_bridge_is_next_to_the_tray() {
    let exe = Path::new("tools").join("push-agent-tray.exe");
    let expected = if cfg!(windows) {
        "push-agent-bridge.exe"
    } else {
        "push-agent-bridge"
    };
    assert_eq!(bridge_exe_next_to(&exe), Path::new("tools").join(expected));
}
