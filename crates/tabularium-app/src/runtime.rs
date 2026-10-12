//! Bridges the immediate-mode UI with the async database layer.
//!
//! UI components never block: they start work with [`Pending::spawn`] and poll the
//! result once per frame. Connections are opened lazily through [`SharedConn`].

use std::future::Future;
use std::sync::mpsc;
use std::sync::{Arc, Mutex as StdMutex};

use tabularium_core::{CancelHandle, Connection, ConnectionConfig, Error, Result};
use tokio::runtime::Handle;

/// Result of a background task, polled from the UI thread.
pub struct Pending<T> {
    rx: mpsc::Receiver<T>,
}

impl<T: Send + 'static> Pending<T> {
    pub fn spawn<F>(rt: &Handle, ctx: &egui::Context, fut: F) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        rt.spawn(async move {
            let out = fut.await;
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        Self { rx }
    }

    /// Returns the result once, when ready.
    pub fn poll(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

/// Poll an optional pending slot; clears it when the result arrives.
pub fn poll_slot<T: Send + 'static>(slot: &mut Option<Pending<T>>) -> Option<T> {
    let out = slot.as_mut()?.poll();
    if out.is_some() {
        *slot = None;
    }
    out
}

/// A lazily-opened database session shared between the UI and background tasks.
///
/// `get()` connects on first use; `disconnect()` closes the session. A config change
/// takes effect on the next connect.
pub struct SharedConn {
    config: StdMutex<ConnectionConfig>,
    /// Password entered at the prompt for configs that don't save it.
    session_password: StdMutex<Option<String>>,
    conn: tokio::sync::Mutex<Option<Arc<dyn Connection>>>,
    /// Mirror of `conn.is_some()` readable without awaiting.
    connected: std::sync::atomic::AtomicBool,
    cancel: StdMutex<Option<Arc<dyn CancelHandle>>>,
    server_version: StdMutex<Option<String>>,
}

impl SharedConn {
    pub fn new(config: ConnectionConfig) -> Arc<Self> {
        Arc::new(Self {
            config: StdMutex::new(config),
            session_password: StdMutex::new(None),
            conn: tokio::sync::Mutex::new(None),
            connected: false.into(),
            cancel: StdMutex::new(None),
            server_version: StdMutex::new(None),
        })
    }

    pub fn config(&self) -> ConnectionConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn set_config(&self, cfg: ConnectionConfig) {
        *self.config.lock().unwrap() = cfg;
    }

    pub fn set_session_password(&self, pw: Option<String>) {
        *self.session_password.lock().unwrap() = pw;
    }

    pub fn has_session_password(&self) -> bool {
        self.session_password.lock().unwrap().is_some()
    }

    /// Effective config used to connect (session password applied).
    pub fn effective_config(&self) -> ConnectionConfig {
        let mut cfg = self.config();
        if let Some(pw) = self.session_password.lock().unwrap().clone() {
            cfg.password = Some(pw);
        }
        cfg
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn server_version(&self) -> Option<String> {
        self.server_version.lock().unwrap().clone()
    }

    /// Get the open session, connecting first if necessary.
    pub async fn get(&self) -> Result<Arc<dyn Connection>> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let cfg = self.effective_config();
        let conn = tabularium_drivers::connect(&cfg).await?;
        if let Ok(v) = conn.server_version().await {
            *self.server_version.lock().unwrap() = Some(v);
        }
        *self.cancel.lock().unwrap() = conn.cancel_handle();
        *guard = Some(conn.clone());
        self.connected.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(conn)
    }

    /// The current session without connecting.
    pub async fn current(&self) -> Option<Arc<dyn Connection>> {
        self.conn.lock().await.clone()
    }

    pub async fn disconnect(&self) {
        let conn = self.conn.lock().await.take();
        self.connected.store(false, std::sync::atomic::Ordering::Relaxed);
        *self.cancel.lock().unwrap() = None;
        if let Some(c) = conn {
            if let Err(e) = c.close().await {
                log::warn!("error closing connection: {e}");
            }
        }
    }

    /// Drop a session that failed in a way that leaves it unusable.
    pub async fn reset_if_broken(&self, err: &Error) {
        if matches!(err, Error::Connection(_)) {
            self.disconnect().await;
        }
    }

    /// Cancel the statement currently running on this session.
    pub fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        self.cancel.lock().unwrap().clone()
    }
}
