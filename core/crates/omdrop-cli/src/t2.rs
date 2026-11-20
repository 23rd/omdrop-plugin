//! KaiT2en owns the radio. Its browse lease lasts as long as the D-Bus client.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use zbus::blocking::{Proxy, connection::Builder};

use crate::proc::run_cancellable;
use crate::send::{PeerJson, is_mac};
use crate::util::{die, is_executable, py_json};

pub fn sender() -> Option<PathBuf> {
    std::env::var_os("OMDROP_T2_SENDER")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let program = PathBuf::from("/usr/lib/omdrop-core/t2drop-send");
            (supported_model()
                && Path::new("/usr/lib/NetworkManager/conf.d/90-t2-drop-awdl.conf").is_file()
                && is_executable(Path::new("/usr/bin/t2drop"))
                && is_executable(&program))
            .then_some(program)
        })
}

pub fn supported_model() -> bool {
    let sysfs =
        std::env::var_os("OMDROP_SYSFS").map_or_else(|| PathBuf::from("/sys"), PathBuf::from);
    std::fs::read_to_string(sysfs.join("class/dmi/id/product_name"))
        .is_ok_and(|model| model.trim() == "MacBookPro15,1")
        && crate::driver::broadcom_wifi_device().as_deref() == Some("0x4464")
}

pub fn check_sender(sender: &Path, address: &str) {
    if !is_mac(&address.to_ascii_lowercase()) {
        die("Choose a device in the panel or pass its full AWDL address with --to.");
    }
    if !is_executable(sender) {
        die(format!(
            "Cannot run the KaiT2en sender at {}.",
            sender.display()
        ));
    }
}

pub fn send(sender: &Path, address: &str, wait: &str, files: &[OsString], verbose: bool) -> i32 {
    check_sender(sender, address);
    if !wait.parse::<u32>().is_ok_and(|seconds| seconds > 0) {
        die("KaiT2en send --wait needs a positive whole number of seconds.");
    }
    let mut command = Command::new(sender);
    if !verbose {
        command.arg("--gui");
    }
    command
        .args(["--to", address, "--wait", wait, "--name"])
        .arg(crate::config::effective_name())
        .arg("--")
        .args(files);
    let description = if files.len() == 1 {
        crate::util::after_last_slash(&files[0])
            .to_string_lossy()
            .into_owned()
    } else {
        format!("{} files", files.len())
    };
    outln!("Waiting for the recipient’s AirDrop…");
    let code = run_cancellable(&mut command);
    if code == 0 {
        outln!("Sent {description}");
    }
    code
}

pub fn peers(args: &[String]) -> i32 {
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let json = has("--json");
    let watch = has("--watch");
    let stream = has("--stream");
    let names = has("-n") || has("--names");
    if args.iter().any(|arg| {
        !matches!(
            arg.as_str(),
            "--json" | "--watch" | "--stream" | "-n" | "--names" | "--no-wake"
        )
    }) || (watch && (!json || stream))
        || (stream && !(json && names))
    {
        die("usage: omdrop peers [-n] [--json [--watch]]");
    }
    // --no-wake reads the existing table without acquiring a browse lease.
    match browse(json, watch, stream, !has("--no-wake")) {
        Ok(()) => 0,
        Err(error) => {
            errln!("KaiT2en discovery failed: {error}");
            1
        }
    }
}

fn browse(json: bool, watch: bool, stream: bool, wake: bool) -> zbus::Result<()> {
    let connection = Builder::system()?
        .method_timeout(Duration::from_secs(15))
        .build()?;
    let proxy = Proxy::new(
        &connection,
        "org.t2drop",
        "/org/t2drop",
        "org.t2drop.Sender1",
    )?;
    if wake {
        proxy.call::<_, _, ()>("Browse", &())?;
    }
    // Closing the connection releases Browse, including when the panel
    // terminates this command. Each snapshot replaces the previous one.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = None;
    let mut compatibility_rows = Vec::new();
    let mut last_probe = None;
    loop {
        let mut peers: Vec<(String, String)> = proxy.call("Receivers", &())?;
        // Some Everyone-mode devices accept direct queries on 8770 while
        // ignoring discovery on the port in their service announcement.
        if wake && last_probe.is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(6)) {
            compatibility_rows = sender()
                .and_then(|path| Command::new(path).arg("--probe-peers").output().ok())
                .filter(|output| output.status.success())
                .and_then(|output| {
                    serde_json::from_slice::<Vec<(String, String)>>(&output.stdout).ok()
                })
                .unwrap_or_default();
            last_probe = Some(Instant::now());
        }
        for (mac, name) in &compatibility_rows {
            if !peers.iter().any(|(address, _)| address == mac) {
                peers.push((mac.clone(), name.clone()));
            }
        }
        let mut rows: Vec<_> = peers
            .into_iter()
            .filter(|(mac, _)| is_mac(mac))
            .map(|(mac, name)| {
                let name: String = name.chars().filter(|c| !c.is_control()).take(255).collect();
                (mac, name.trim().to_owned())
            })
            .collect();
        rows.sort_unstable();
        rows.dedup_by(|a, b| a.0 == b.0);
        if !watch && (rows.is_empty() && wake && Instant::now() < deadline) {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if let Some(text) =
            snapshot(&rows, json, stream).filter(|text| previous.as_ref() != Some(text))
        {
            let mut stdout = io::stdout().lock();
            if writeln!(stdout, "{text}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                break;
            }
            previous = Some(text);
        }
        if !watch {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn snapshot(rows: &[(String, String)], json: bool, stream: bool) -> Option<String> {
    if json {
        let rows: Vec<_> = rows
            .iter()
            .map(|(mac, name)| PeerJson {
                mac,
                rssi: None,
                name: (!name.is_empty()).then_some(name),
            })
            .collect();
        if !stream {
            return Some(py_json(&rows));
        }
        return (!rows.is_empty()).then(|| rows.iter().map(py_json).collect::<Vec<_>>().join("\n"));
    }
    (!rows.is_empty()).then(|| {
        rows.iter()
            .map(|(mac, name)| format!("{mac} {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

const RECEIVE_UNIT: &str = "omdrop-t2-receive.service";

pub fn receiver() -> Option<PathBuf> {
    let path = std::env::var_os("OMDROP_T2_RECEIVER")
        .map(PathBuf::from)
        .or_else(|| sender().map(|sender| sender.with_file_name("t2drop-receive")))?;
    is_executable(&path).then_some(path)
}

pub fn receive_status() -> Option<(&'static str, Option<i64>)> {
    use crate::proc::capture_ok;
    let output = capture_ok(
        "systemctl",
        &[
            "--user",
            "show",
            RECEIVE_UNIT,
            "-p",
            "ActiveState",
            "-p",
            "ExecStart",
            "-p",
            "RuntimeMaxUSec",
            "-p",
            "ActiveEnterTimestampMonotonic",
        ],
    )?;
    let field = |key: &str| {
        output.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name == key).then_some(value)
        })
    };
    if field("ActiveState")? != "active" {
        return None;
    }
    if field("ExecStart").is_some_and(|s| s.contains(" --once ;")) {
        return Some(("once", None));
    }
    let remaining = field("RuntimeMaxUSec")
        .and_then(crate::status::span_microseconds)
        .zip(field("ActiveEnterTimestampMonotonic").and_then(|s| s.parse::<u64>().ok()))
        .and_then(|(limit, start)| start.checked_add(limit))
        .map(|until| {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: now points to a writable timespec.
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
            let now = now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1000;
            (until.saturating_sub(now) / 1_000_000) as i64
        });
    Some((
        if remaining.is_some() {
            "timed"
        } else {
            "forever"
        },
        remaining,
    ))
}

pub fn receive_window(subcommand: &str, args: &[String]) -> i32 {
    use crate::proc::run;
    let Some(program) = receiver() else {
        die("The KaiT2en desktop receiver is missing.");
    };
    let active = receive_status().is_some();
    if matches!(subcommand, "off" | "stop") || (subcommand == "toggle" && active) {
        return run("systemctl", &["--user", "stop", RECEIVE_UNIT]);
    }
    if active {
        die("Turn receiving off before opening a new AirDrop window.");
    }
    if crate::status::receiving() {
        die("The original Omdrop receiver is running. Stop airdrop-receiver.service first.");
    }
    let duration = args
        .first()
        .map(String::as_str)
        .unwrap_or(crate::ctx::DEFAULT_WINDOW);
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--quiet",
        "--collect",
        "--unit",
        RECEIVE_UNIT,
        "--property=Type=notify",
        "--property=NotifyAccess=all",
        "--property=TimeoutStartSec=25s",
        "--property=TimeoutStopSec=4s",
    ]);
    if !matches!(duration, "once" | "forever") {
        let (_, seconds) =
            crate::window::parse_duration(duration).unwrap_or_else(|error| die(error));
        if seconds == 0 {
            die("Choose a positive receiving duration or forever.");
        }
        command.arg(format!("--property=RuntimeMaxSec={seconds}s"));
    }
    command
        .arg("--")
        .arg(program)
        .arg(format!("--name={}", crate::config::effective_name()))
        .arg(format!("--dir={}", crate::config::effective_dir()));
    if duration == "once" {
        command.arg("--once");
    }
    let status = command
        .status()
        .map_or(1, |status| status.code().unwrap_or(1));
    if status != 0 {
        errln!("KaiT2en could not start receiving. See the AirDrop service log.");
    }
    status
}
