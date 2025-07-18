/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An event loop implementation that works in headless mode.

use std::sync::{Arc, Condvar, Mutex};
use std::time;

use log::warn;
use servo::EventLoopWaker;
use winit::error::EventLoopError;
use winit::event_loop::EventLoop as WinitEventLoop;
#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

use super::app::App;

pub type EventLoopProxy = winit::event_loop::EventLoopProxy<AppEvent>;

#[derive(Debug)]
pub enum AppEvent {
    /// Another process or thread has kicked the OS event loop with EventLoopWaker.
    Waker,
    Accessibility(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for AppEvent {
    fn from(event: accesskit_winit::Event) -> AppEvent {
        AppEvent::Accessibility(event)
    }
}

/// The real or fake OS event loop.
#[allow(dead_code)]
enum EventLoop {
    /// A real Winit windowing event loop.
    Winit(winit::event_loop::EventLoop<AppEvent>),
    /// A fake event loop which contains a signalling flag used to ensure
    /// that pending events get processed in a timely fashion, and a condition
    /// variable to allow waiting on that flag changing state.
    Headless(Arc<(Mutex<bool>, Condvar)>),
}

pub struct EventsLoop(EventLoop);

impl EventsLoop {
    // Ideally, we could use the winit event loop in both modes,
    // but on Linux, the event loop requires a X11 server.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub fn new(_headless: bool, _has_output_file: bool) -> Result<EventsLoop, EventLoopError> {
        Ok(EventsLoop(EventLoop::Winit(
            WinitEventLoop::with_user_event().build()?,
        )))
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn new(headless: bool, _has_output_file: bool) -> Result<EventsLoop, EventLoopError> {
        Ok(EventsLoop(if headless {
            EventLoop::Headless(Arc::new((Mutex::new(false), Condvar::new())))
        } else {
            EventLoop::Winit(WinitEventLoop::with_user_event().build()?)
        }))
    }
    #[cfg(target_os = "macos")]
    pub fn new(headless: bool, _has_output_file: bool) -> Result<EventsLoop, EventLoopError> {
        Ok(EventsLoop(if headless {
            EventLoop::Headless(Arc::new((Mutex::new(false), Condvar::new())))
        } else {
            let mut event_loop_builder = WinitEventLoop::with_user_event();
            if _has_output_file {
                // Prevent the window from showing in Dock.app, stealing focus,
                // when generating an output file.
                event_loop_builder.with_activation_policy(ActivationPolicy::Prohibited);
            }
            EventLoop::Winit(event_loop_builder.build()?)
        }))
    }
}

impl EventsLoop {
    pub(crate) fn event_loop_proxy(&self) -> Option<EventLoopProxy> {
        match self.0 {
            EventLoop::Winit(ref events_loop) => Some(events_loop.create_proxy()),
            EventLoop::Headless(..) => None,
        }
    }

    pub fn create_event_loop_waker(&self) -> Box<dyn EventLoopWaker> {
        match self.0 {
            EventLoop::Winit(ref events_loop) => Box::new(HeadedEventLoopWaker::new(events_loop)),
            EventLoop::Headless(ref data) => Box::new(HeadlessEventLoopWaker(data.clone())),
        }
    }

    pub fn run_app(self, app: &mut App) {
        match self.0 {
            EventLoop::Winit(events_loop) => {
                events_loop
                    .run_app(app)
                    .expect("Failed while running events loop");
            },
            EventLoop::Headless(ref data) => {
                let (flag, condvar) = &**data;

                app.init(None);
                loop {
                    self.sleep(flag, condvar);
                    app.handle_webdriver_messages();
                    if !app.handle_events_with_headless() {
                        break;
                    }
                    if !app.animating() {
                        *flag.lock().unwrap() = false;
                    }
                }
            },
        }
    }

    fn sleep(&self, lock: &Mutex<bool>, condvar: &Condvar) {
        // To avoid sleeping when we should be processing events, do two things:
        // * before sleeping, check whether our signalling flag has been set
        // * wait on a condition variable with a maximum timeout, to allow
        //   being woken up by any signals that occur while sleeping.
        let guard = lock.lock().unwrap();
        if *guard {
            return;
        }
        let _ = condvar
            .wait_timeout(guard, time::Duration::from_millis(5))
            .unwrap();
    }
}

struct HeadedEventLoopWaker {
    proxy: Arc<Mutex<winit::event_loop::EventLoopProxy<AppEvent>>>,
}
impl HeadedEventLoopWaker {
    fn new(events_loop: &winit::event_loop::EventLoop<AppEvent>) -> HeadedEventLoopWaker {
        let proxy = Arc::new(Mutex::new(events_loop.create_proxy()));
        HeadedEventLoopWaker { proxy }
    }
}
impl EventLoopWaker for HeadedEventLoopWaker {
    fn wake(&self) {
        // Kick the OS event loop awake.
        if let Err(err) = self.proxy.lock().unwrap().send_event(AppEvent::Waker) {
            warn!("Failed to wake up event loop ({}).", err);
        }
    }
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(HeadedEventLoopWaker {
            proxy: self.proxy.clone(),
        })
    }
}

struct HeadlessEventLoopWaker(Arc<(Mutex<bool>, Condvar)>);
impl EventLoopWaker for HeadlessEventLoopWaker {
    fn wake(&self) {
        // Set the signalling flag and notify the condition variable.
        // This ensures that any sleep operation is interrupted,
        // and any non-sleeping operation will have a change to check
        // the flag before going to sleep.
        let (ref flag, ref condvar) = *self.0;
        let mut flag = flag.lock().unwrap();
        *flag = true;
        condvar.notify_all();
    }
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(HeadlessEventLoopWaker(self.0.clone()))
    }
}
