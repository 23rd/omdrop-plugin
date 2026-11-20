use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::json;
use zbus::blocking::{Proxy, connection::Builder};
use zbus::names::BusName;

const ADDRESS: &str = "02:00:00:00:00:01";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct BrowseState {
    client: String,
    peers: Vec<(String, String)>,
}

struct Sender(Arc<Mutex<BrowseState>>);

#[zbus::interface(name = "org.t2drop.Sender1")]
impl Sender {
    fn browse(&self, #[zbus(header)] header: zbus::message::Header<'_>) {
        self.0.lock().unwrap().client = header.sender().unwrap().to_string();
    }

    fn receivers(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().peers.clone()
    }
}

fn launcher() -> Command {
    let mut cmd = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bin/omdrop"));
    cmd.env("OMDROP_CORE", env!("CARGO_BIN_EXE_omdrop"))
        .env("OMDROP_IMPLEMENTATION", "rust");
    cmd
}

#[test]
fn watch_replaces_peer_snapshots_and_releases_the_browse_client() {
    let mut daemon = Process(
        Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut address = String::new();
    BufReader::new(daemon.0.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let state = Arc::new(Mutex::new(BrowseState {
        peers: vec![(ADDRESS.into(), "Test tablet".into())],
        ..BrowseState::default()
    }));
    let connection = Builder::address(address.trim())
        .unwrap()
        .name("org.t2drop")
        .unwrap()
        .serve_at("/org/t2drop", Sender(state.clone()))
        .unwrap()
        .build()
        .unwrap();
    let mut watch = Process(
        launcher()
            .env("OMDROP_T2_SENDER", "/unused/test-sender")
            .env("DBUS_SYSTEM_BUS_ADDRESS", address.trim())
            .args(["peers", "--json", "--watch"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (write, read) = mpsc::channel();
    let stdout = watch.0.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if write.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || {
        serde_json::from_str::<serde_json::Value>(
            &read.recv_timeout(Duration::from_secs(5)).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        next(),
        json!([{"mac": ADDRESS, "name": "Test tablet", "rssi": null}])
    );

    let client = state.lock().unwrap().client.clone();
    let client = BusName::try_from(client.as_str()).unwrap();
    let dbus = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .unwrap();
    let connected = || {
        dbus.call::<_, _, bool>("NameHasOwner", &(client.clone(),))
            .unwrap()
    };
    assert!(connected());
    state.lock().unwrap().peers.clear();
    assert_eq!(next(), json!([]));
    // The panel signals the CLI when the radar closes.
    assert_eq!(
        unsafe { libc::kill(watch.0.id().try_into().unwrap(), libc::SIGTERM) },
        0
    );
    watch.0.wait().unwrap();
    reader.join().unwrap();
    assert!(!connected());
}

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn send_pick_keeps_the_selected_address_and_reports_the_sender_result() {
    let tmp = tempfile::tempdir().unwrap();
    let picker = tmp.path().join("picker");
    let sender = tmp.path().join("sender");
    let file = tmp.path().join("file with spaces.txt");
    std::fs::write(&file, "AirDrop test").unwrap();
    executable(
        &picker,
        "#!/bin/sh\nif [ \"$CANCEL_PICK\" = 1 ]; then exit 1; fi\nprintf '%s\\0' \"$TEST_FILE\"\n",
    );
    executable(
        &sender,
        r#"#!/bin/sh
[ "$1" = --gui ] && [ "$2" = --to ] && [ "$3" = "$ADDRESS" ] &&
[ "$8" = -- ] && [ "$9" = "$TEST_FILE" ] || exit 64
[ "$DECLINE" = 1 ] && { echo 'The recipient declined the transfer.' >&2; exit 3; }
exit 0
"#,
    );
    let transfer = |cancel: bool, decline: bool| {
        launcher()
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .env("XDG_STATE_HOME", tmp.path())
            .env("OMDROP_T2_SENDER", &sender)
            .env("OMDROP_PICKER", &picker)
            .env("ADDRESS", ADDRESS)
            .env("TEST_FILE", &file)
            .env("CANCEL_PICK", if cancel { "1" } else { "0" })
            .env("DECLINE", if decline { "1" } else { "0" })
            .args(["send", "--pick", "--to", ADDRESS])
            .output()
            .unwrap()
    };
    let done = transfer(false, false);
    assert!(done.status.success(), "{done:?}");
    assert!(String::from_utf8_lossy(&done.stdout).contains("Sent file with spaces.txt"));
    let declined = transfer(false, true);
    assert_eq!(declined.status.code(), Some(3));
    assert!(!String::from_utf8_lossy(&declined.stdout).contains("Sent "));
    assert!(String::from_utf8_lossy(&declined.stderr).contains("declined"));
    let cancelled = transfer(true, false);
    assert_eq!(cancelled.status.code(), Some(1));
    assert!(cancelled.stdout.is_empty(), "{cancelled:?}");
    assert!(cancelled.stderr.is_empty(), "{cancelled:?}");
}

#[test]
fn provider_firewall_rule_uses_kait2en_port_for_install_and_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let path = std::env::join_paths(
        std::iter::once(tmp.path().to_owned())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    executable(&tmp.path().join("systemctl"), "#!/bin/sh\nexit 0\n");
    executable(&tmp.path().join("ufw"), "#!/bin/sh\nexit 0\n");
    executable(
        &tmp.path().join("sudo"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\"\n",
    );
    for (action, prefix) in [
        ("install", "ufw allow"),
        ("remove", "ufw --force delete allow"),
    ] {
        let result = launcher()
            .env("PATH", &path)
            .env("OMDROP_T2_SENDER", "/unused/test-sender")
            .args(["firewall", action])
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(
            output.contains(&format!(
                "{prefix} in on awdl0 from fe80::/10 to fe80::/10 port 8770 proto tcp"
            )),
            "{output}"
        );
        assert!(!output.contains("8771"), "{output}");
    }
}
