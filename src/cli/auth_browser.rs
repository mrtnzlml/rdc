//! `rdc auth <env> --browser` — take a token through a one-time local web
//! form instead of the command line or stdin.
//!
//! Built for callers with no terminal, such as Claude Code's `!`
//! commands: the token never appears in argv, shell history or the
//! command's output, so it never reaches an agent's transcript.
//!
//! rdc binds `127.0.0.1` on a free port and serves the form under a random
//! path, so only the browser it opened (or someone who read the printed URL)
//! can post to it. It validates the token the same way `--token` does, saves
//! it, and exits. A rejected token re-shows the form. Five minutes without a
//! valid token ends the run with an error.

use crate::config::EnvConfig;
use crate::log::Action;
use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(300);

pub async fn run(env_cfg: &EnvConfig, project_root: &Path, env: &str) -> Result<String> {
    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
    let listener = TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let secret = random_hex()?;
    let path = format!("/{secret}");
    let url = format!("http://127.0.0.1:{port}{path}");

    log.event(Action::Auth, &format!("open {url} to enter the token for env '{env}'"));
    open_browser(&url);

    let deadline = Instant::now() + TIMEOUT;
    let mut error: Option<String> = None;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow!("no token entered within {} s; run `rdc auth {env} --browser` again", TIMEOUT.as_secs()));
        }
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e).context("accepting a connection"),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let Ok((method, target, body)) = read_request(&mut stream) else {
            continue;
        };
        if target != path {
            respond(&mut stream, "404 Not Found", "not found");
            continue;
        }
        match method.as_str() {
            "GET" => respond(&mut stream, "200 OK", &form_page(env, env_cfg, error.as_deref())),
            "POST" => {
                let token = form_value(&body, "token").unwrap_or_default();
                let token = token.trim();
                if token.is_empty() {
                    error = Some("Paste a token before you save.".into());
                    redirect(&mut stream, &path);
                    continue;
                }
                match crate::cli::auth::validate_and_save_token(env_cfg, project_root, env, token).await {
                    Ok(org) => {
                        respond(&mut stream, "200 OK", &done_page(env, &org));
                        return Ok(org);
                    }
                    Err(e) => {
                        error = Some(format!("Rossum rejected this token. Check that it belongs to this org.\n{e:#}"));
                        redirect(&mut stream, &path);
                    }
                }
            }
            _ => respond(&mut stream, "405 Method Not Allowed", "method not allowed"),
        }
    }
}

fn random_hex() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("reading OS randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    // Best effort: the URL is already printed for a headless machine.
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Read one HTTP/1.1 request: method, path and body. Enough for a browser
/// posting a small form; nothing more.
fn read_request(stream: &mut TcpStream) -> Result<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(anyhow!("connection closed"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(anyhow!("headers too large"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
        .min(64 * 1024);
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((method, target, String::from_utf8_lossy(&body).to_string()))
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
         script-src-attr 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Post/redirect/get, so a browser reload never re-sends the token.
fn redirect(stream: &mut TcpStream, path: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 303 See Other\r\nLocation: {path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const STYLE: &str = r#"
:root{--paper:#E9EEF2;--card:#F7F9FB;--ink:#0F1C26;--slate:#546675;--rule:#C5D0D9;
--signal:#1F5FD6;--on-signal:#FFFFFF;--ok:#1B7A4A;--err:#B42318;--err-bg:#FBECEA;color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--paper:#0E161D;--card:#15202A;--ink:#E6EDF2;--slate:#93A4B2;
--rule:#2A3843;--signal:#6E9BFF;--on-signal:#0E161D;--ok:#5CC48D;--err:#FF8A7A;--err-bg:#2B1715}}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;background:var(--paper);color:var(--ink);
font:16px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;display:grid;place-items:center;padding:2rem 1rem}
main{width:100%;max-width:30rem}
.status{display:flex;align-items:center;gap:.6rem;color:var(--slate);font-size:.9375rem;margin:0 0 1.25rem}
.dot{width:.625rem;height:.625rem;border-radius:50%;background:var(--signal);flex:none}
.waiting .dot{animation:pulse 1.6s ease-in-out infinite}
.saved .dot{background:var(--ok)}
@keyframes pulse{50%{opacity:.25}}
@media (prefers-reduced-motion:reduce){.waiting .dot{animation:none}}
h1{font:600 clamp(2rem,7vw,2.75rem)/1.1 ui-monospace,"SF Mono",Menlo,Consolas,monospace;letter-spacing:-.02em;margin:0 0 .75rem;overflow-wrap:anywhere}
h1 .prompt{color:var(--slate);font-weight:400}
.target{margin:0 0 2rem;color:var(--slate)}
.target b{color:var(--ink);font-weight:600;overflow-wrap:anywhere}
form{background:var(--card);border:1px solid var(--rule);border-radius:12px;padding:1.25rem}
label{display:block;font-weight:600;margin-bottom:.5rem}
.field{display:flex;gap:.5rem}
input{flex:1;min-width:0;font:1rem ui-monospace,"SF Mono",Menlo,Consolas,monospace;color:var(--ink);background:var(--paper);
border:1px solid var(--rule);border-radius:8px;padding:.7rem .8rem}
button{font:inherit;font-weight:600;border-radius:8px;cursor:pointer}
.toggle{background:none;border:1px solid var(--rule);color:var(--slate);padding:0 .9rem}
.save{width:100%;margin-top:1rem;background:var(--signal);color:var(--on-signal);border:0;padding:.75rem}
input:focus-visible,button:focus-visible{outline:3px solid var(--signal);outline-offset:2px}
.hint{color:var(--slate);font-size:.875rem;margin:.75rem 0 0}
.error{background:var(--err-bg);border-left:3px solid var(--err);border-radius:4px;padding:.75rem .9rem;margin:0 0 1rem}
.error strong{color:var(--err);display:block}
.error small{color:var(--slate);overflow-wrap:anywhere}
.foot{color:var(--slate);font-size:.875rem;margin:1.5rem 0 0}
code{font:.875em ui-monospace,"SF Mono",Menlo,Consolas,monospace}
"#;

fn page(env: &str, class: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>rdc auth {env}</title><style>{STYLE}</style>\
         <main class={class}>{body}</main></html>",
        env = escape(env),
    )
}

fn form_page(env: &str, env_cfg: &EnvConfig, error: Option<&str>) -> String {
    let err = error
        .map(|e| {
            let (head, detail) = e.split_once('\n').unwrap_or((e, ""));
            format!(
                "<div class=error role=alert><strong>{}</strong><small>{}</small></div>",
                escape(head),
                escape(detail)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<p class=status><span class=dot></span>Your terminal is waiting for this token.</p>\
         <h1><span class=prompt>$ </span>rdc auth {env}</h1>\
         <p class=target>Org <b>{org}</b> at <b>{api}</b></p>\
         <form method=post autocomplete=off>{err}\
         <label for=token>API token</label>\
         <div class=field><input id=token name=token type=password autofocus required spellcheck=false>\
         <button type=button class=toggle aria-controls=token \
         onclick=\"const i=document.getElementById('token');const s=i.type==='password';\
         i.type=s?'text':'password';this.textContent=s?'Hide':'Show'\">Show</button></div>\
         <button class=save>Save token</button>\
         <p class=hint>rdc checks the token with Rossum before it saves it.</p></form>\
         <p class=foot>The token goes to <code>secrets/{env}.secrets.json</code> and never to the terminal. \
         rdc stops waiting {mins} minutes after it started.</p>",
        env = escape(env),
        org = env_cfg.org_id,
        api = escape(&env_cfg.api_base),
        mins = TIMEOUT.as_secs() / 60,
    );
    page(env, "waiting", &body)
}

fn done_page(env: &str, org: &str) -> String {
    let body = format!(
        "<p class=status><span class=dot></span>Token saved. You can close this tab.</p>\
         <h1><span class=prompt>$ </span>rdc auth {env}</h1>\
         <p class=target>Signed in to <b>{org}</b>. Return to your terminal to continue.</p>",
        env = escape(env),
        org = escape(org),
    );
    page(env, "saved", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_value_decodes_percent_and_plus() {
        assert_eq!(form_value("a=1&token=ab%2Fc+d", "token").as_deref(), Some("ab/c d"));
        assert_eq!(form_value("a=1", "token"), None);
    }

    #[test]
    fn percent_decode_keeps_malformed_escapes() {
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%C3%A9"), "é");
    }

    #[test]
    fn pages_escape_what_they_echo() {
        let page = done_page("<env>", "a&b");
        assert!(page.contains("&lt;env&gt;") && page.contains("a&amp;b"));
        assert!(!page.contains("<env>"));
    }

    #[test]
    fn secret_path_is_random_hex() {
        let (a, b) = (random_hex().unwrap(), random_hex().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
