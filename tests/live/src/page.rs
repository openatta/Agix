//! A web site for `WebFetch` (and hooks of the `http` kind) to reach: a local
//! HTTP server whose pages a case registers, and which keeps every request
//! it was sent.
//!
//! The record is the point. A model can answer from what it already knows,
//! or say it fetched a page it never asked for; a hit on a path only the case
//! knew is the evidence that the fetch really happened, and a page whose
//! content the case made up at runtime is the evidence its answer came from
//! there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use crate::provider::read_request;

/// What a path answers with.
#[derive(Clone, Debug)]
pub enum Page {
    Body {
        status: u16,
        content_type: String,
        body: String,
    },
    Redirect {
        status: u16,
        location: String,
    },
    /// This page, sent only after the wait.
    Slow(Duration, Box<Page>),
}

impl Page {
    pub fn html(body: impl Into<String>) -> Self {
        Self::with_type("text/html; charset=utf-8", body)
    }

    pub fn text(body: impl Into<String>) -> Self {
        Self::with_type("text/plain; charset=utf-8", body)
    }

    pub fn json(value: &serde_json::Value) -> Self {
        Self::with_type("application/json", value.to_string())
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Body {
            status,
            content_type: "text/plain; charset=utf-8".into(),
            body: body.into(),
        }
    }

    /// A 302 to `location` — a path on this server or any URL.
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::Redirect {
            status: 302,
            location: location.into(),
        }
    }

    /// A plain-text page of `chars` characters with each `(at, mark)` placed
    /// at character offset `at`: a mark early on survives a fetch's
    /// truncation, one far in does not.
    pub fn huge(chars: usize, marks: &[(usize, &str)]) -> Self {
        let filler = "lorem ipsum dolor sit amet ";
        let mut body: Vec<char> = filler.chars().cycle().take(chars).collect();
        for (at, mark) in marks {
            let mark: Vec<char> = mark.chars().collect();
            let at = (*at).min(body.len().saturating_sub(mark.len()));
            body.splice(at..at + mark.len(), mark);
        }
        Self::text(body.into_iter().collect::<String>())
    }

    pub fn slow(wait: Duration, page: Page) -> Self {
        Self::Slow(wait, Box::new(page))
    }

    fn with_type(content_type: &str, body: impl Into<String>) -> Self {
        Self::Body {
            status: 200,
            content_type: content_type.into(),
            body: body.into(),
        }
    }
}

#[derive(Clone)]
pub struct PageServer {
    base: url::Url,
    routes: Arc<Mutex<HashMap<String, Page>>>,
    hits: Arc<Mutex<Vec<String>>>,
}

impl PageServer {
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = Self {
            base: url::Url::parse(&format!("http://127.0.0.1:{port}/"))?,
            routes: Arc::default(),
            hits: Arc::default(),
        };
        let serving = server.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let server = serving.clone();
                tokio::spawn(async move {
                    let _ = server.serve_one(sock).await;
                });
            }
        });
        Ok(server)
    }

    /// `http://127.0.0.1:<port>` + `path`.
    pub fn url(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.base.as_str().trim_end_matches('/'),
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        )
    }

    /// Serve `page` at `path` (no query string), replacing what was there.
    /// Anything unregistered is a 404.
    pub fn route(&self, path: &str, page: Page) {
        lock(&self.routes).insert(path.to_string(), page);
    }

    /// How many requests reached `path`, whatever their query string.
    pub fn hits(&self, path: &str) -> usize {
        lock(&self.hits)
            .iter()
            .filter(|p| strip_query(p) == path)
            .count()
    }

    /// Every request target so far, query string included, in order.
    pub fn requests(&self) -> Vec<String> {
        lock(&self.hits).clone()
    }

    async fn serve_one(&self, mut sock: tokio::net::TcpStream) -> anyhow::Result<()> {
        let Some(request) = read_request(&mut sock).await? else {
            return Ok(());
        };
        lock(&self.hits).push(request.path.clone());
        let page = lock(&self.routes).get(strip_query(&request.path)).cloned();
        let mut page = page.unwrap_or_else(|| Page::status(404, "not found"));
        loop {
            match page {
                Page::Slow(wait, inner) => {
                    tokio::time::sleep(wait).await;
                    page = *inner;
                }
                Page::Redirect { status, location } => {
                    let location = if location.starts_with('/') {
                        self.url(&location)
                    } else {
                        location
                    };
                    let head = format!(
                        "HTTP/1.1 {status} Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    sock.write_all(head.as_bytes()).await?;
                    break;
                }
                Page::Body {
                    status,
                    content_type,
                    body,
                } => {
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    sock.write_all(head.as_bytes()).await?;
                    if request.method != "HEAD" {
                        sock.write_all(body.as_bytes()).await?;
                    }
                    break;
                }
            }
        }
        sock.flush().await?;
        let _ = sock.shutdown().await;
        Ok(())
    }
}

fn strip_query(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or(target)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
