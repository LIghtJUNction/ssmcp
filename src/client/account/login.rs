//! Interactive account authorization. Only public links and results reach the UI.
use super::{Arguments, Url, begin_authorization, finish_authorization};
use std::{future::Future, path::PathBuf};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client) enum Mode {
    Open,
    Copy,
    Link,
}

impl Mode {
    pub(in crate::client) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "" | "open" => Ok(Self::Open),
            "copy" => Ok(Self::Copy),
            "link" => Ok(Self::Link),
            _ => Err("Use /login [open|copy|link]".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) struct Link {
    pub url: String,
    pub confirmation: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::client) enum Event {
    Link(Link),
    Notice(String),
    Finished(Result<String, String>),
}

pub(in crate::client) struct Task {
    actions: UnboundedSender<Mode>,
    events: UnboundedReceiver<Event>,
    handle: tokio::task::JoinHandle<()>,
}

impl Task {
    pub(in crate::client) fn start(mode: Mode) -> Result<Self, String> {
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| {
                crate::client::interface::harness::local_runtime()
                    .ok()
                    .map(|(_, handle)| handle)
            })
            .ok_or("GitHub login requires an active client runtime")?;
        let (actions, receiver) = mpsc::unbounded_channel();
        let (sender, events) = mpsc::unbounded_channel();
        let handle = runtime.spawn(async move {
            // Resolving configuration, HTTP and desktop actions all happen after
            // releasing the terminal lock. No Credential is sent to the UI.
            let result = match super::state_root() {
                Ok(root) => {
                    run_at(
                        root,
                        Arguments::default(),
                        mode,
                        receiver,
                        &sender,
                        desktop_action,
                    )
                    .await
                }
                Err(error) => Err(super::safe(&error.to_string())),
            };
            publish(&sender, Event::Finished(result));
        });
        Ok(Self {
            actions,
            events,
            handle,
        })
    }

    pub(in crate::client) async fn cancel_and_wait(mut self) {
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }

    pub(in crate::client) fn request(&self, mode: Mode) -> Result<(), String> {
        self.actions
            .send(mode)
            .map_err(|_| "This login has finished; run /login again".into())
    }

    pub(in crate::client) fn poll(&mut self) -> Option<Event> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => Some(Event::Finished(Err(
                "GitHub login stopped; run /login again".into(),
            ))),
        }
    }

    #[cfg(test)]
    pub(in crate::client) fn pending_for_test()
    -> (Self, UnboundedReceiver<Mode>, UnboundedSender<Event>) {
        let (actions, receiver) = mpsc::unbounded_channel();
        let (sender, events) = mpsc::unbounded_channel();
        (
            Self {
                actions,
                events,
                handle: tokio::spawn(std::future::pending()),
            },
            receiver,
            sender,
        )
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn publish(events: &UnboundedSender<Event>, event: Event) -> bool {
    let sent = events.send(event).is_ok();
    if sent {
        crate::client::terminal::invalidate();
    }
    sent
}

async fn desktop_action(mode: Mode, url: Url) -> Result<&'static str, String> {
    match mode {
        Mode::Open => super::open_browser(&url)
            .await
            .map(|()| "Browser opening requested · the sign-in link is in Conversation")
            .map_err(|_| "Cannot open a browser · use /login copy or /login link".into()),
        Mode::Copy => crate::client::clipboard::write_text(url.as_str())
            .await
            .map(|()| "GitHub login link copied to the system clipboard")
            .map_err(|error| format!("{} · use /login link", super::safe(&error))),
        Mode::Link => {
            Ok("GitHub login link shown in Conversation · complete sign-in in your browser")
        }
    }
}

async fn run_at<F, Effect>(
    root: PathBuf,
    args: Arguments,
    first: Mode,
    mut actions: UnboundedReceiver<Mode>,
    events: &UnboundedSender<Event>,
    mut effect: F,
) -> Result<String, String>
where
    F: FnMut(Mode, Url) -> Effect,
    Effect: Future<Output = Result<&'static str, String>>,
{
    let authorization = begin_authorization(&root, &args, None)
        .await
        .map_err(|error| super::safe(&error.to_string()))?;
    let link = Link {
        url: authorization.url.to_string(),
        confirmation: authorization.user_code.clone(),
    };
    let url = authorization.url.clone();
    let deadline = authorization.deadline;
    let finish = finish_authorization(&root, authorization);
    tokio::pin!(finish);
    let mut next = Some(first);
    loop {
        if let Some(mode) = next.take() {
            if tokio::time::Instant::now() >= deadline {
                return Err("GitHub login link expired; run /login again".into());
            }
            if !publish(events, Event::Link(link.clone())) {
                return Err("GitHub login cancelled".into());
            }
            let result = tokio::select! {
                result = &mut finish => return result
                    .map(|credential| credential.principal)
                    .map_err(|error| super::safe(&error.to_string())),
                result = effect(mode, url.clone()) => result,
            };
            let message = match result {
                Ok(message) => message.to_owned(),
                Err(message) => message,
            };
            if !publish(events, Event::Notice(message)) {
                return Err("GitHub login cancelled".into());
            }
        }
        tokio::select! {
            result = &mut finish => return result
                .map(|credential| credential.principal)
                .map_err(|error| super::safe(&error.to_string())),
            action = actions.recv() => match action {
                Some(mode) => next = Some(mode),
                None => return Err("GitHub login cancelled".into()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        net::TcpListener,
        sync::Notify,
        time::{Duration, timeout},
    };

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let mut random = [0; 16];
            crate::platform::random_bytes(&mut random).unwrap();
            let root =
                std::env::temp_dir().join(format!("ssmcp-login-{}", u128::from_le_bytes(random)));
            super::super::create_tree(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn modes_accept_only_explicit_local_actions() {
        assert_eq!(Mode::parse(""), Ok(Mode::Open));
        assert_eq!(Mode::parse("open"), Ok(Mode::Open));
        assert_eq!(Mode::parse("copy"), Ok(Mode::Copy));
        assert_eq!(Mode::parse("link"), Ok(Mode::Link));
        for invalid in [
            "github",
            "https://other.example",
            "copy extra",
            "--token secret",
        ] {
            assert!(Mode::parse(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn repeated_actions_reuse_one_grant_and_only_publish_public_login_data() {
        let root = Temporary::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{server}/setup?code=PUBLIC-CODE");
        let starts = Arc::new(AtomicUsize::new(0));
        let complete = Arc::new(Notify::new());
        let app = Router::new()
            .route(
                "/api/client/login/start",
                post({
                    let starts = starts.clone();
                    let url = url.clone();
                    move || {
                        let starts = starts.clone();
                        let url = url.clone();
                        async move {
                            starts.fetch_add(1, Ordering::SeqCst);
                            Json(
                                json!({"device_code":"d".repeat(64),"user_code":format!("{}\n{}", "d".repeat(32), "d".repeat(32)),
                            "verification_uri_complete":url,"expires_in":60,"interval":1}),
                            )
                        }
                    }
                }),
            )
            .route(
                "/api/client/login/poll",
                post({
                    let complete = complete.clone();
                    move |Json(request): Json<serde_json::Value>| {
                        let complete = complete.clone();
                        async move {
                            assert_eq!(request["device_code"], "d".repeat(64));
                            complete.notified().await;
                            Json(json!({"status":"authorized","access_token":"a".repeat(64),
                            "expires_in":3600,"identity":{"provider":"github","subject":"123",
                            "login":"alice","principal":"alice@github"}}))
                        }
                    }
                }),
            );
        let service = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (actions, receiver) = mpsc::unbounded_channel();
        let (sender, mut events) = mpsc::unbounded_channel();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let recorded = effects.clone();
        let args = Arguments {
            server: Some(server.clone()),
            name: Some("isolated login test".into()),
            ..Arguments::default()
        };
        let path = root.0.clone();
        let worker = tokio::spawn(async move {
            run_at(
                path,
                args,
                Mode::Open,
                receiver,
                &sender,
                move |mode, url| {
                    recorded.lock().unwrap().push((mode, url.to_string()));
                    std::future::ready(if mode == Mode::Open {
                        Err("Browser unavailable in the isolated fixture".into())
                    } else {
                        Ok("Test desktop action completed")
                    })
                },
            )
            .await
        });
        let mut links = Vec::new();
        for mode in [Mode::Open, Mode::Copy, Mode::Link] {
            if mode != Mode::Open {
                actions.send(mode).unwrap();
            }
            loop {
                let event = timeout(Duration::from_secs(3), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let visible = format!("{event:?}");
                assert!(!visible.contains(&"d".repeat(64)));
                assert!(!visible.contains(&"a".repeat(64)));
                match event {
                    Event::Link(link) => links.push(link),
                    Event::Notice(message) => {
                        if mode == Mode::Open {
                            assert!(message.contains("Browser unavailable"));
                        }
                        break;
                    }
                    Event::Finished(_) => {
                        panic!("authorization completed before browser confirmation")
                    }
                }
            }
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(links.len(), 3);
        assert!(
            links
                .iter()
                .all(|link| link.url == url && link.confirmation.is_none())
        );
        assert_eq!(
            *effects.lock().unwrap(),
            vec![
                (Mode::Open, url.clone()),
                (Mode::Copy, url.clone()),
                (Mode::Link, url)
            ]
        );
        assert!(
            super::super::load_at(&root.0, Some(&server), false)
                .unwrap()
                .is_none()
        );
        complete.notify_one();
        let result = timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, Ok("alice@github".into()));
        let saved = super::super::load_at(&root.0, Some(&server), false)
            .unwrap()
            .unwrap();
        assert_eq!(saved.principal, "alice@github");
        assert_eq!(saved.subject.as_deref(), Some("123"));
        service.abort();
    }

    #[tokio::test]
    async fn unsafe_verification_url_never_reaches_desktop_or_ui() {
        let root = Temporary::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/api/client/login/start",
            post(|| async {
                Json(
                    json!({"device_code":"d".repeat(64),"user_code":"PUBLIC-CODE",
                "verification_uri_complete":"https://other.example/setup?code=PUBLIC-CODE",
                "expires_in":60,"interval":1}),
                )
            }),
        );
        let service = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (_actions, receiver) = mpsc::unbounded_channel();
        let (sender, mut events) = mpsc::unbounded_channel();
        let args = Arguments {
            server: Some(server.clone()),
            name: Some("isolated rejection test".into()),
            ..Arguments::default()
        };
        let result = run_at(
            root.0.clone(),
            args,
            Mode::Copy,
            receiver,
            &sender,
            |_, _| {
                panic!("an unsafe login link must not invoke a desktop helper");
                #[allow(unreachable_code)]
                std::future::ready(Ok("unused"))
            },
        )
        .await;
        assert!(result.unwrap_err().contains("unsafe login URL"));
        assert!(events.try_recv().is_err());
        assert!(
            super::super::load_at(&root.0, Some(&server), false)
                .unwrap()
                .is_none()
        );
        service.abort();
    }
}
