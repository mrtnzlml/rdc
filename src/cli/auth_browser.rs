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

use crate::config::ProjectConfig;
use crate::log::Action;
use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(300);

pub async fn run(cfg: &ProjectConfig, project_root: &Path, env: &str) -> Result<String> {
    let env_cfg = cfg.env_or_err(env)?;
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
    let mut rejection: Option<Rejection> = None;
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
            "GET" => {
                let left = deadline.saturating_duration_since(Instant::now()).as_secs();
                respond(&mut stream, "200 OK", &form_page(cfg, env, left, rejection.as_ref()))
            }
            "POST" => {
                let token = form_value(&body, "token").unwrap_or_default();
                let token = token.trim();
                if token.is_empty() {
                    rejection = Some(Rejection::empty());
                    redirect(&mut stream, &path);
                    continue;
                }
                match crate::cli::auth::validate_and_save_token(env_cfg, project_root, env, token).await {
                    Ok(org) => {
                        respond(&mut stream, "200 OK", &done_page(cfg, env, &org));
                        return Ok(org);
                    }
                    Err(e) => {
                        rejection = Some(Rejection::from_error(&e, env_cfg.org_id));
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
    // A fresh nonce per response lets the page's one `<script>` run and
    // nothing else.
    let nonce = random_hex().unwrap_or_default();
    let body = body.replace(NONCE, &nonce);
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
         script-src 'nonce-{nonce}'; connect-src 'self'; \
         form-action 'self'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
:root{--paper:#F5F6F9;--field:#FFFFFF;--ink:#1A1D33;--muted:#6B7090;--faint:#A3A7C0;--rule:#E1E3EC;
--console:#1C1F3B;--console-ink:#D9DCF5;--console-dim:#7E83AD;--cursor:#F5B841;--console-err:#FF9A8D;
--action:#3F44D6;--on-action:#FFFFFF;--ok:#3FBF86;--err:#C2362B;
--mono:ui-monospace,"SF Mono",Menlo,Consolas,monospace;--sans:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--paper:#101225;--field:#171A31;--ink:#E8EAF7;--muted:#9499BD;--faint:#5D628A;
--rule:#262A48;--console:#080913;--console-dim:#6E739E;--action:#8F93FF;--on-action:#101225;--ok:#62D6A0;--err:#FF8A7D}}
*{box-sizing:border-box}
html,body{margin:0;min-height:100%}
body{background:var(--paper);color:var(--ink);font:15px/1.55 var(--sans);-webkit-font-smoothing:antialiased}
#app{position:relative;min-height:100vh;overflow:hidden;isolation:isolate;padding:4.5rem 1.5rem;--col:14.5rem;--gap:15.5rem}
main{max-width:26rem;margin:0 auto}
.ghost{position:absolute;z-index:-1;top:9.25rem;width:var(--col);margin:0;overflow:hidden;pointer-events:none;user-select:none;
font:11.5px/2 var(--mono);color:color-mix(in srgb,var(--ink) 7%,transparent);white-space:pre;
-webkit-mask-image:linear-gradient(to right,#000 62%,transparent);mask-image:linear-gradient(to right,#000 62%,transparent)}
.ghost b{font-weight:400;color:color-mix(in srgb,var(--ink) 15%,transparent)}
.ghost.read{left:calc(50% - var(--gap) - var(--col))}
.ghost.wrote{left:calc(50% + var(--gap))}
.ghost.wrote b{color:color-mix(in srgb,var(--ok) 30%,transparent)}
.timeout .ghost{display:none}
@media (max-width:1000px){.ghost{display:none}}
.console{display:flex;align-items:baseline;gap:.6rem;background:var(--console);color:var(--console-ink);
border-radius:8px;padding:.8rem 1rem;font:13px/1.5 var(--mono);margin:0 0 3rem}
.console .ps,.console .time{color:var(--console-dim)}
.console .time{font-variant-numeric:tabular-nums}
.console .msg{flex:1;color:var(--console-dim);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.console .ok{color:var(--ok)}
.console .err{color:var(--console-err)}
.cursor{display:inline-block;width:.55em;height:1.05em;vertical-align:-.18em;background:var(--cursor);margin-left:.4em;
animation:blink 1.1s steps(1) infinite}
@keyframes blink{50%{opacity:0}}
@media (prefers-reduced-motion:reduce){.cursor{animation:none}}
h1{font-size:1.5rem;line-height:1.25;font-weight:600;letter-spacing:-.02em;margin:0 0 .35rem;overflow-wrap:anywhere}
.sub{color:var(--muted);margin:0 0 2rem;overflow-wrap:anywhere}
.sub b{color:var(--ink);font-weight:600}
.field{display:flex;align-items:center;background:var(--field);border:1px solid var(--rule);border-radius:8px;
transition:border-color .15s,box-shadow .15s}
.field:focus-within{border-color:var(--action);box-shadow:0 0 0 4px color-mix(in srgb,var(--action) 14%,transparent)}
.field.bad{border-color:var(--err)}
.field input{flex:1;min-width:0;border:0;background:none;color:var(--ink);font:14px var(--mono);
letter-spacing:.04em;padding:.85rem 1rem;outline:none}
.field input::placeholder{color:var(--faint);font-family:var(--sans);letter-spacing:0}
.field button{border:0;background:none;color:var(--muted);font:inherit;font-size:13px;padding:.4rem .9rem;cursor:pointer}
.field button:hover{color:var(--ink)}
.error{color:var(--err);font-size:13.5px;margin:.6rem 0 0;overflow-wrap:anywhere}
.save{display:block;width:100%;margin:1rem 0 0;border:0;border-radius:8px;background:var(--action);color:var(--on-action);
font:inherit;font-weight:600;padding:.75rem;cursor:pointer}
.save:hover{filter:brightness(1.06)}
.save:disabled{opacity:.55;cursor:progress}
button:focus-visible{outline:2px solid var(--action);outline-offset:3px;border-radius:6px}
.note{color:var(--faint);font-size:13px;margin:1.25rem 0 0;text-align:center}
.note.left{text-align:left;margin-top:0}
.note code{font:12px var(--mono);color:var(--muted)}
.cmd{font:13px var(--mono);color:var(--ink);background:var(--field);border:1px solid var(--rule);border-radius:8px;padding:.75rem 1rem;margin:0}
"#;

const NONCE: &str = "__RDC_NONCE__";

/// Submits the form with `fetch()` and swaps `#app` in place, so the tab
/// keeps a single history entry. Browsers let a script close only such a
/// tab, so after a save it closes itself. The countdown mirrors rdc's
/// deadline (`data-left`, in seconds) and swaps in the timed-out view at
/// zero. Without script the form posts normally and every page still reads
/// correctly; only the countdown stays still.
const SCRIPT: &str = r#"
let timer;
function arm(){
const app=document.getElementById('app');clearInterval(timer);
const left=+app.dataset.left;if(!left)return;
const end=Date.now()+left*1000,time=app.querySelector('.time');
timer=setInterval(()=>{
const s=Math.max(0,Math.round((end-Date.now())/1000));
if(time)time.textContent=Math.floor(s/60)+':'+String(s%60).padStart(2,'0');
if(!s){clearInterval(timer);const t=document.getElementById('timeout');
app.className='timeout';app.querySelector('main').replaceWith(t.content.cloneNode(true))}
},1000);
}
arm();
document.addEventListener('click',e=>{
const b=e.target.closest('.toggle');if(!b)return;
const i=document.getElementById('token');const s=i.type==='password';
i.type=s?'text':'password';b.textContent=s?'Hide':'Show';
});
document.addEventListener('submit',async e=>{
const f=e.target;e.preventDefault();const b=f.querySelector('.save');b.disabled=true;b.textContent='Checking with Rossum…';
let html;
try{const r=await fetch(location.pathname,{method:'POST',body:new URLSearchParams(new FormData(f))});html=await r.text()}
catch(_){f.submit();return}
const next=new DOMParser().parseFromString(html,'text/html').getElementById('app');
document.getElementById('app').replaceWith(document.adoptNode(next));
if(next.classList.contains('saved')){
const n=next.querySelector('.note');setTimeout(()=>{window.close();n.textContent='You can close this tab.'},3000);
}else{arm();const t=document.getElementById('token');if(t)t.focus()}
});
"#;

/// What the page reports after a token was refused.
struct Rejection {
    /// Short state for the console line, e.g. `rejected (401)`.
    console: String,
    /// One sentence under the field.
    message: String,
}

impl Rejection {
    fn from_error(e: &anyhow::Error, org_id: u64) -> Self {
        for status in [401u16, 403] {
            if crate::api::anyhow_has_status(e, status) {
                return Rejection {
                    console: format!("rejected ({status})"),
                    message: format!(
                        "Rossum rejected this token. Check that it belongs to org {org_id} and has not expired."
                    ),
                };
            }
        }
        Rejection { console: "check failed".into(), message: format!("rdc could not check the token: {e:#}") }
    }

    fn empty() -> Self {
        Rejection { console: "no token sent".into(), message: "Paste a token before you save.".into() }
    }
}

/// The host of `api_base`, or `api_base` itself when it does not parse.
fn host(api_base: &str) -> String {
    reqwest::Url::parse(api_base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| api_base.to_string())
}

/// The left ghost: the envs of `rdc.toml`, with the one being set darker.
/// Only names, API bases and org ids, all of which the page shows anyway.
fn ghost_config(cfg: &ProjectConfig, env: &str) -> String {
    let blocks: Vec<String> = cfg
        .envs
        .iter()
        .map(|(name, e)| {
            let block = format!(
                "[envs.{}]\napi_base = \"{}\"\norg_id = {}",
                escape(name),
                escape(&e.api_base),
                e.org_id
            );
            if name == env { format!("<b>{block}</b>") } else { block }
        })
        .collect();
    format!("<pre class=\"ghost read\" aria-hidden=true># rdc.toml\n\n{}</pre>", blocks.join("\n\n"))
}

/// The right ghost: the secrets file, empty until the save, then with a
/// masked value. Never the token itself.
fn ghost_secrets(env: &str, saved: bool) -> String {
    let line = if saved { "<b>\"api_token\": \"••••••••••••••••\"</b>" } else { "\"api_token\": \"\"" };
    format!(
        "<pre class=\"ghost wrote\" aria-hidden=true># secrets/{}.secrets.json\n\n{{\n  {line}\n}}</pre>",
        escape(env)
    )
}

fn console(env: &str, msg: &str, class: &str, left: Option<u64>) -> String {
    let (cursor, time) = match left {
        Some(s) => ("<span class=cursor></span>", format!("<span class=time>{}:{:02}</span>", s / 60, s % 60)),
        None => ("", String::new()),
    };
    format!(
        "<div class=console aria-live=polite><span class=ps>$</span><span>rdc auth {}</span>\
         <span class=\"msg {class}\">{}{cursor}</span>{time}</div>",
        escape(env),
        escape(msg)
    )
}

fn page(env: &str, class: &str, left: u64, ghosts: &str, main: &str) -> String {
    let timeout = format!(
        "<main>{}<h1>rdc stopped waiting</h1>\
         <p class=sub>Nothing was saved. Run the command again to open a new form.</p>\
         <p class=cmd>rdc auth {env}</p></main>",
        console(env, &format!("timed out after {} min", TIMEOUT.as_secs() / 60), "err", None),
        env = escape(env),
    );
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>rdc auth {env}</title><style>{STYLE}</style>\
         <div id=app class={class} data-left={left}>{ghosts}<main>{main}</main></div>\
         <template id=timeout>{timeout}</template>\
         <script nonce={NONCE}>{SCRIPT}</script></html>",
        env = escape(env),
    )
}

fn form_page(cfg: &ProjectConfig, env: &str, left: u64, rejection: Option<&Rejection>) -> String {
    let env_cfg = &cfg.envs[env];
    let (msg, msg_class) = match rejection {
        Some(r) => (r.console.as_str(), "err"),
        None => ("waiting", ""),
    };
    let err = rejection
        .map(|r| format!("<p class=error role=alert id=token-error>{}</p>", escape(&r.message)))
        .unwrap_or_default();
    let invalid = if rejection.is_some() { " bad" } else { "" };
    let aria = if rejection.is_some() { " aria-invalid=true aria-describedby=token-error" } else { "" };
    let main = format!(
        "{console}<h1>Paste the API token for {env}</h1>\
         <p class=sub>Org {org} on {host}</p>\
         <form method=post autocomplete=off>\
         <div class=\"field{invalid}\"><input id=token name=token type=password placeholder=\"API token\" \
         aria-label=\"API token\" autofocus required spellcheck=false{aria}>\
         <button type=button class=toggle aria-controls=token>Show</button></div>{err}\
         <button class=save>Save token</button></form>\
         <p class=note>Checked with Rossum, then saved to <code>secrets/{env}.secrets.json</code></p>",
        console = console(env, msg, msg_class, Some(left)),
        env = escape(env),
        org = env_cfg.org_id,
        host = escape(&host(&env_cfg.api_base)),
    );
    let class = if rejection.is_some() { "rejected" } else { "waiting" };
    let ghosts = ghost_config(cfg, env) + &ghost_secrets(env, false);
    page(env, class, left, &ghosts, &main)
}

fn done_page(cfg: &ProjectConfig, env: &str, org: &str) -> String {
    let main = format!(
        "{console}<h1>Token saved</h1>\
         <p class=sub>It reads <b>{org}</b> ({id}). You can go back to the terminal.</p>\
         <p class=\"note left\">This tab closes in 3 seconds.</p>",
        console = console(env, &format!("saved, org '{org}'"), "ok", None),
        org = escape(org),
        id = cfg.envs[env].org_id,
    );
    let ghosts = ghost_config(cfg, env) + &ghost_secrets(env, true);
    page(env, "saved", 0, &ghosts, &main)
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

    fn config(envs: &[(&str, u64)]) -> ProjectConfig {
        ProjectConfig {
            envs: envs
                .iter()
                .map(|(n, id)| {
                    (n.to_string(), crate::config::EnvConfig { api_base: "https://example.rossum.app/api/v1".into(), org_id: *id })
                })
                .collect(),
        }
    }

    #[test]
    fn pages_escape_what_they_echo() {
        let page = done_page(&config(&[("dev", 1)]), "dev", "a&b<i>");
        assert!(page.contains("a&amp;b&lt;i&gt;") && !page.contains("a&b<i>"));
    }

    #[test]
    fn form_shows_the_org_and_host_and_ghosts_every_env() {
        let page = form_page(&config(&[("dev", 11), ("prod", 22)]), "dev", 299, None);
        assert!(page.contains("Org 11 on example.rossum.app"), "{page}");
        assert!(page.contains("<b>[envs.dev]") && page.contains("[envs.prod]") && !page.contains("<b>[envs.prod]"));
        assert!(page.contains("data-left=299") && page.contains("4:59"));
        assert!(page.contains("\"api_token\": \"\"") && !page.contains("••"));
    }

    #[test]
    fn done_page_masks_the_token_and_stops_the_clock() {
        let page = done_page(&config(&[("dev", 1)]), "dev", "Acme Dev");
        assert!(page.contains("class=saved data-left=0") && page.contains("••••"));
    }

    #[test]
    fn a_401_reads_as_a_rejection_and_anything_else_keeps_its_cause() {
        let e = anyhow::Error::new(crate::api::ApiError::Status {
            status: 401,
            body: "{}".into(),
        });
        let r = Rejection::from_error(&e, 7);
        assert_eq!(r.console, "rejected (401)");
        assert!(r.message.contains("org 7"));
        let r = Rejection::from_error(&anyhow::anyhow!("connection refused"), 7);
        assert_eq!(r.console, "check failed");
        assert!(r.message.contains("connection refused"));
    }

    #[test]
    fn secret_path_is_random_hex() {
        let (a, b) = (random_hex().unwrap(), random_hex().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
