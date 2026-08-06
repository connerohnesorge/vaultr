use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

use crate::secrets::Policy;

use super::engine::ScannedFinding;
use super::input::ScanInput;
use super::review::{self, ReviewState};

const REVIEW_HTML: &str = include_str!("review.html");

fn http_json(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn http_html(stream: &mut TcpStream) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        REVIEW_HTML.len(),
        REVIEW_HTML
    )?;
    stream.flush()?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(anyhow!("browser closed the review request"));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = end + 4;
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err(anyhow!("review request headers are too large"));
        }
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let first = headers
        .lines()
        .next()
        .ok_or_else(|| anyhow!("review request has no request line"))?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
        })
        .map(str::trim)
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(anyhow!("browser closed the review request body"));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok((
        method,
        target,
        bytes[header_end..header_end + length].to_vec(),
    ))
}

pub(super) fn serve_review(
    input: &ScanInput,
    policy: &mut Policy,
    findings: Vec<ScannedFinding>,
) -> Result<i32> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/", listener.local_addr()?);
    println!("secret review: {url}");
    let _ = Command::new("open").arg(&url).status();
    let mut state = ReviewState::new(findings);

    loop {
        let (mut stream, _) = listener.accept()?;
        let result = (|| -> Result<Option<i32>> {
            let (method, target, body) = read_request(&mut stream)?;
            let path = target.split('?').next().unwrap_or(&target);
            match (method.as_str(), path) {
                ("GET", "/") => {
                    http_html(&mut stream)?;
                    Ok(None)
                }
                ("GET", "/api/findings") => {
                    let body = serde_json::to_string(&state.view())?;
                    http_json(&mut stream, "200 OK", &body)?;
                    Ok(None)
                }
                ("POST", "/api/action") => {
                    let (body, exit) = review::action(input, policy, &mut state, &body)?;
                    http_json(&mut stream, "200 OK", &body)?;
                    Ok(exit)
                }
                _ => {
                    http_json(
                        &mut stream,
                        "404 Not Found",
                        r#"{"ok":false,"message":"not found"}"#,
                    )?;
                    Ok(None)
                }
            }
        })();
        match result {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => {}
            Err(error) => {
                let body = serde_json::json!({
                    "ok": false,
                    "message": error.to_string()
                })
                .to_string();
                let _ = http_json(&mut stream, "400 Bad Request", &body);
            }
        }
    }
}
