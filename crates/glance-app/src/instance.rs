//! One glance per identity. A second launch (for example clicking a
//! `glance://` link while glance runs in the tray) hands its link to the
//! running app over a local socket and exits.
//!
//! The running app writes `instance.json` (port + random token) to its data
//! dir; only processes that can read that file can talk to it.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a second launch asks the running app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Show,
    Join(String),
}

impl Request {
    fn encode(&self) -> String {
        match self {
            Request::Show => "show".into(),
            Request::Join(link) => format!("join {link}"),
        }
    }

    fn decode(line: &str) -> Option<Self> {
        match line.trim() {
            "show" => Some(Request::Show),
            l => l
                .strip_prefix("join ")
                .map(|link| Request::Join(link.trim().to_string())),
        }
    }

    /// From command-line arguments: a `glance://` link means join.
    pub fn from_args(args: &[String]) -> Self {
        args.iter()
            .find(|a| a.starts_with("glance://"))
            .map(|l| Request::Join(l.clone()))
            .unwrap_or(Request::Show)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Info {
    port: u16,
    token: String,
}

fn info_path(dir: &Path) -> PathBuf {
    dir.join("instance.json")
}

/// Try to pass `req` to an already running glance. True if it took it.
pub fn hand_off(dir: &Path, req: &Request) -> bool {
    let Ok(text) = std::fs::read_to_string(info_path(dir)) else {
        return false;
    };
    let Ok(info) = serde_json::from_str::<Info>(&text) else {
        return false;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(stream, "{}\n{}", info.token, req.encode()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// Become the running instance: accept hand-offs and send them to `tx`.
/// The listener thread lives as long as the process.
pub fn listen(dir: &Path, tx: flume::Sender<Request>) -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let token: String = rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let info = Info {
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    std::fs::create_dir_all(dir)?;
    let path = info_path(dir);
    std::fs::write(&path, serde_json::to_vec(&info)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = serve(stream, &token, &tx);
            }
        })?;
    Ok(())
}

fn serve(stream: TcpStream, token: &str, tx: &flume::Sender<Request>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let (mut got_token, mut line) = (String::new(), String::new());
    reader.read_line(&mut got_token)?;
    reader.read_line(&mut line)?;
    if got_token.trim() != token {
        return Ok(());
    }
    if let Some(req) = Request::decode(&line) {
        let _ = tx.send(req);
        (&stream).write_all(b"ok\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_hands_off_to_the_first() {
        let dir = std::env::temp_dir().join(format!("glance-instance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Nobody running yet.
        assert!(!hand_off(&dir, &Request::Show));
        let (tx, rx) = flume::unbounded();
        listen(&dir, tx).unwrap();
        let link = Request::Join("glance://join/abc".into());
        assert!(hand_off(&dir, &link));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), link);
        assert!(hand_off(&dir, &Request::Show));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Request::Show
        );
        // A wrong token is ignored.
        std::fs::write(info_path(&dir), r#"{"port":1,"token":"x"}"#).unwrap();
        assert!(!hand_off(&dir, &Request::Show));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn args_pick_the_link() {
        let args = vec!["--hidden".to_string(), "glance://join/xyz".to_string()];
        assert_eq!(
            Request::from_args(&args),
            Request::Join("glance://join/xyz".into())
        );
        assert_eq!(Request::from_args(&[]), Request::Show);
    }
}
