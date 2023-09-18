use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use colored::Colorize;
use futures::{SinkExt, StreamExt};
use hyper::header::CONTENT_TYPE;
use hyper::http::HeaderValue;
use hyper::Server;
use regex::Regex;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::try_join;
use tracing::debug;
use tungstenite::Message;

use crate::compiler;
use crate::compiler::Compiler;
use crate::watch::watch;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub struct DevServer {
    watcher: Arc<ProjectWatch>,
    compiler: Arc<Compiler>,
}

impl DevServer {
    pub fn new(root: PathBuf, compiler: Arc<Compiler>) -> Self {
        Self {
            watcher: Arc::new(ProjectWatch::new(root, compiler.clone())),
            compiler,
        }
    }

    pub async fn serve(&self) {
        let watch_handler = self.watcher.start();

        async fn serve_websocket(
            websocket: hyper_tungstenite::HyperWebsocket,
            mut rx: Receiver<WsMessage>,
        ) -> Result<(), Error> {
            let websocket = websocket.await?;

            let (mut sender, mut ws_recv) = websocket.split();

            let fwd_task = tokio::spawn(async move {
                loop {
                    if let Ok(msg) = rx.recv().await {
                        if sender
                            .send(Message::text(format!(r#"{{"hash":"{}"}}"#, msg.hash)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });

            while let Some(message) = ws_recv.next().await {
                if let Ok(Message::Close(_)) = message {
                    break;
                }
            }

            // release rx;
            fwd_task.abort();

            Ok(())
        }
        let arc_watcher = self.watcher.clone();
        let compiler = self.compiler.clone();
        let handle_request = move |mut req: hyper::Request<hyper::Body>| {
            let for_fn = compiler.clone();
            let w = arc_watcher.clone();
            async move {
                let path = req.uri().path().strip_prefix('/').unwrap_or("");

                let static_serve =
                    hyper_staticfile::Static::new(for_fn.context.config.output.path.clone());

                let static_serve_hmr = hyper_staticfile::Static::new(
                    for_fn.context.root.join("node_modules/.mako/hot_update"),
                );

                // 去除 publicPath 头尾 /
                let public_path = for_fn.context.config.public_path.clone();
                let public_path_without_fix =
                    public_path.trim_start_matches('/').trim_end_matches('/');

                match path {
                    "__/hmr-ws" => {
                        if hyper_tungstenite::is_upgrade_request(&req) {
                            let (response, websocket) =
                                hyper_tungstenite::upgrade(req, None).unwrap();

                            tokio::spawn(async move {
                                if let Err(e) = serve_websocket(websocket, w.clone_receiver()).await
                                {
                                    eprintln!("Error in websocket connection: {}", e);
                                }
                            });

                            Ok(response)
                        } else {
                            Ok::<_, hyper::Error>(
                                hyper::Response::builder()
                                    .status(hyper::StatusCode::NOT_FOUND)
                                    .body(hyper::Body::empty())
                                    .unwrap(),
                            )
                        }
                    }
                    _ if path.starts_with(public_path_without_fix) => {
                        // 如果用户设置了 public_path，修改一下原始 req，手动复制 req 担心掉属性
                        if !public_path.is_empty() {
                            let public_path_re = Regex::new(public_path_without_fix).unwrap();
                            let uri_str = public_path_re
                                .replacen(req.uri().to_string().as_str(), 1, "")
                                .to_string();
                            let uri_cloned = uri_str.as_str().parse::<hyper::Uri>().unwrap();
                            *req.uri_mut() = uri_cloned;
                        };

                        // clone 一份 req，用于做 hmr 的匹配
                        let herders_cloned = req.headers().clone();
                        let uri_cloned = req.uri().clone();
                        let mut req_cloned = hyper::Request::builder()
                            .method(hyper::Method::GET)
                            .uri(uri_cloned)
                            .body(hyper::Body::empty())
                            .unwrap();
                        req_cloned.headers_mut().extend(herders_cloned);

                        // 先匹配 hmr 请求静态资源请求，用复制的 req
                        let static_serve_hmr_result = static_serve_hmr.serve(req_cloned).await;
                        let serve_result = match static_serve_hmr_result {
                            Ok(res) => {
                                if res.status() == hyper::StatusCode::OK {
                                    Ok(res)
                                } else {
                                    static_serve.serve(req).await
                                }
                            }
                            _ => static_serve.serve(req).await,
                        };

                        // 后续处理
                        match serve_result {
                            Ok(mut res) => {
                                if let Some(content_type) = res.headers().get(CONTENT_TYPE).cloned()
                                {
                                    if let Ok(c_str) = content_type.to_str() {
                                        if c_str.contains("javascript") || c_str.contains("text") {
                                            res.headers_mut()
                                                .insert(
                                                    CONTENT_TYPE,
                                                    HeaderValue::from_str(&format!(
                                                        "{c_str}; charset=utf-8"
                                                    ))
                                                    .unwrap(),
                                                )
                                                .unwrap();
                                        }
                                    }
                                }
                                Ok(res)
                            }
                            Err(_) => Ok::<_, hyper::Error>(
                                hyper::Response::builder()
                                    .status(hyper::StatusCode::NOT_FOUND)
                                    .body(hyper::Body::from("404 - Page not found"))
                                    .unwrap(),
                            ),
                        }
                    }
                    _ => Ok::<_, hyper::Error>(
                        hyper::Response::builder()
                            .status(hyper::StatusCode::NOT_FOUND)
                            .body(hyper::Body::from("404 - Page not found"))
                            .unwrap(),
                    ),
                }
            }
        };
        let dev_service = hyper::service::make_service_fn(move |_conn| {
            let my_fn = handle_request.clone();
            async move { Ok::<_, hyper::Error>(hyper::service::service_fn(my_fn)) }
        });

        let port = self.compiler.context.config.hmr_port.clone();
        let port = port.parse::<u16>().unwrap();
        let dev_server_handle = tokio::spawn(async move {
            if let Err(_e) = Server::bind(&([127, 0, 0, 1], port).into())
                .serve(dev_service)
                .await
            {
                println!("done");
            }
        });

        // build_handle 必须在 dev_server_handle 之前
        // 否则会导致 build_handle 无法收到前几个消息，原因未知
        let join_error = try_join!(watch_handler, dev_server_handle);
        if let Err(e) = join_error {
            eprintln!("Error in dev server: {:?}", e);
        }
    }
}

#[derive(Clone, Debug)]
struct WsMessage {
    hash: u64,
}

struct ProjectWatch {
    root: PathBuf,
    compiler: std::sync::Arc<compiler::Compiler>,
    tx: Sender<WsMessage>,
}

impl ProjectWatch {
    pub fn new(root: PathBuf, c: Arc<compiler::Compiler>) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel::<WsMessage>(256);
        Self {
            compiler: c,
            root,
            tx,
        }
    }

    pub fn start(&self) -> JoinHandle<()> {
        let c = self.compiler.clone();
        let root = self.root.clone();
        let tx = self.tx.clone();

        let mut last_full_hash = Box::new(c.full_hash());

        let watch_compiler = c.clone();

        tokio::spawn(async move {
            watch(&root, |events| {
                println!("Compiling...");
                let res = watch_compiler.update(events.into());
                let has_no_missing_deps = {
                    watch_compiler
                        .context
                        .modules_with_missing_deps
                        .read()
                        .unwrap()
                        .is_empty()
                };

                match res {
                    Err(err) => {
                        // unescape
                        let mut err = err
                            .to_string()
                            .replace("\\n", "\n")
                            .replace("\\u{1b}", "\u{1b}")
                            .replace("\\\\", "\\");
                        // remove first char and last char
                        if err.starts_with('"') && err.ends_with('"') {
                            err = err[1..err.len() - 1].to_string();
                        }
                        eprintln!("{}", "Build failed.".to_string().red());
                        eprintln!("{}", err);
                    }
                    Ok(res) => {
                        if res.is_updated() {
                            let t_compiler = Instant::now();
                            let next_full_hash =
                                watch_compiler.generate_hot_update_chunks(res, *last_full_hash);

                            if has_no_missing_deps {
                                println!(
                                    "Hot rebuilt in {}",
                                    format!("{}ms", t_compiler.elapsed().as_millis()).bold()
                                );
                            }

                            if let Err(e) = next_full_hash {
                                eprintln!("Error in watch: {:?}", e);
                                return;
                            }

                            let next_full_hash = next_full_hash.unwrap();

                            debug!(
                                "Updated: {:?} {:?} {}",
                                next_full_hash,
                                last_full_hash,
                                next_full_hash == *last_full_hash
                            );
                            if next_full_hash == *last_full_hash {
                                // no need to continue
                                return;
                            } else {
                                *last_full_hash = next_full_hash;
                            }

                            if let Err(e) = c.emit_dev_chunks() {
                                debug!("Error in build: {:?}, will rebuild soon", e);
                                return;
                            }
                            if has_no_missing_deps {
                                println!(
                                    "Full rebuilt in {}",
                                    format!("{}ms", t_compiler.elapsed().as_millis()).bold()
                                );
                            }

                            debug!("receiver count: {}", tx.receiver_count());
                            if tx.receiver_count() > 0 {
                                tx.send(WsMessage {
                                    hash: next_full_hash,
                                })
                                .unwrap();
                            }
                        }
                    }
                }
            })
            .await;
        })
    }

    pub fn clone_receiver(&self) -> Receiver<WsMessage> {
        self.tx.subscribe()
    }
}
