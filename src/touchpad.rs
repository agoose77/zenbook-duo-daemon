use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use evdev_rs::{
    Device, DeviceWrapper as _, GrabMode, InputEvent, ReadFlag, UInputDevice,
    enums::{BusType, EV_ABS, EV_KEY, EV_SYN, EventCode},
};
use futures::stream::StreamExt;
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};
use nix::libc;
use tokio::{fs, task::spawn_blocking};

use crate::{
    config::{Config, TouchpadConfig},
    idle_detection::ActivityNotifier,
    mpris::MprisTracker,
    user_commands::UserCommandRunner,
};

const TOUCHPAD_VENDOR_ID: u16 = 0x0b05;
const TOUCHPAD_PRODUCT_ID: u16 = 0x1bf2;
const TOUCHPAD_VERSION: u16 = 0x0111;
const VIRTUAL_TOUCHPAD_NAME: &str = "Zenbook Duo Daemon Touchpad";
const TAP_TIMEOUT: Duration = Duration::from_millis(350);
const TAP_MOVEMENT_THRESHOLD: f64 = 0.025;
const STEP_EPSILON: f64 = 0.000_001;

pub fn start_touchpad_monitor_task(
    config: &Config,
    activity_notifier: ActivityNotifier,
    mpris_tracker: MprisTracker,
    command_runner: UserCommandRunner,
) {
    if !config.touchpad.enabled {
        info!("Touchpad remapping is disabled");
        return;
    }

    let touchpad_config = config.touchpad.clone();
    tokio::spawn(async move {
        let mut entries = match fs::read_dir("/dev/input").await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to read /dev/input for touchpad discovery: {}", e);
                return;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            try_start_touchpad_task(
                touchpad_config.clone(),
                entry.path(),
                activity_notifier.clone(),
                mpris_tracker.clone(),
                command_runner.clone(),
            )
            .await;
        }

        let inotify = Inotify::init().expect("Failed to initialize inotify");
        inotify
            .watches()
            .add("/dev/input/", WatchMask::CREATE)
            .expect("Failed to add inotify watch for touchpad discovery");

        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer).unwrap();

        while let Some(event_result) = stream.next().await {
            if let Ok(event) = event_result {
                if !event.mask.contains(inotify::EventMask::CREATE) {
                    continue;
                }

                let Some(name) = event.name else {
                    continue;
                };

                if name.to_str().unwrap_or("").starts_with("event") {
                    try_start_touchpad_task(
                        touchpad_config.clone(),
                        PathBuf::from("/dev/input/").join(name),
                        activity_notifier.clone(),
                        mpris_tracker.clone(),
                        command_runner.clone(),
                    )
                    .await;
                }
            }
        }
    });
}

async fn try_start_touchpad_task(
    config: TouchpadConfig,
    path: PathBuf,
    activity_notifier: ActivityNotifier,
    mpris_tracker: MprisTracker,
    command_runner: UserCommandRunner,
) {
    if !is_event_device(&path).await {
        return;
    }

    let path_clone = path.clone();
    let input = spawn_blocking(move || {
        let file = std::fs::File::open(&path_clone)
            .map_err(|e| format!("Failed to open {}: {}", path_clone.display(), e))?;
        Device::new_from_file(file).map_err(|e| {
            format!(
                "Failed to create evdev device for {}: {}",
                path_clone.display(),
                e
            )
        })
    })
    .await
    .unwrap();

    let mut input = match input {
        Ok(input) => input,
        Err(e) => {
            debug!("{}", e);
            return;
        }
    };

    if !matches_touchpad(&input, &config) {
        return;
    }

    let Some(geometry) = TouchpadGeometry::from_device(&input) else {
        warn!(
            "Touchpad {} matched, but ABS coordinate ranges were not available",
            path.display()
        );
        return;
    };

    let physical_name = input.name().unwrap_or("unknown").to_string();
    input.set_name(VIRTUAL_TOUCHPAD_NAME);
    input.set_bustype(BusType::BUS_VIRTUAL as u16);

    let virtual_touchpad = match UInputDevice::create_from_device(&input) {
        Ok(device) => device,
        Err(e) => {
            warn!(
                "Failed to create virtual touchpad for {}: {}",
                path.display(),
                e
            );
            return;
        }
    };

    if let Err(e) = input.grab(GrabMode::Grab) {
        warn!("Failed to grab touchpad {}: {}", path.display(), e);
        return;
    }

    start_touchpad_task(
        config,
        path,
        physical_name,
        input,
        virtual_touchpad,
        geometry,
        activity_notifier,
        mpris_tracker,
        command_runner,
    );
}

async fn is_event_device(path: &PathBuf) -> bool {
    if let Ok(metadata) = fs::metadata(path).await {
        if metadata.is_dir() {
            return false;
        }
    } else {
        return false;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("event"))
}

fn matches_touchpad(input: &Device, config: &TouchpadConfig) -> bool {
    if input.name() == Some(VIRTUAL_TOUCHPAD_NAME) || input.bustype() == BusType::BUS_VIRTUAL as u16
    {
        return false;
    }

    let name_matches = input.name() == Some(config.device_name.as_str());
    let id_matches = input.bustype() == BusType::BUS_USB as u16
        && input.vendor_id() == TOUCHPAD_VENDOR_ID
        && input.product_id() == TOUCHPAD_PRODUCT_ID
        && input.version() == TOUCHPAD_VERSION;

    name_matches || id_matches
}

#[allow(clippy::too_many_arguments)]
fn start_touchpad_task(
    config: TouchpadConfig,
    path: PathBuf,
    physical_name: String,
    touchpad: Device,
    virtual_touchpad: UInputDevice,
    geometry: TouchpadGeometry,
    activity_notifier: ActivityNotifier,
    mpris_tracker: MprisTracker,
    command_runner: UserCommandRunner,
) {
    info!(
        "Touchpad remapping connected on {} ({})",
        path.display(),
        physical_name
    );

    let tap_generation = Arc::new(AtomicU64::new(0));
    let mut engine = GestureEngine::new(config.clone(), geometry);
    let touchpad = Arc::new(std::sync::Mutex::new(touchpad));

    tokio::spawn(async move {
        loop {
            let touchpad_clone = touchpad.clone();
            let result = spawn_blocking(move || {
                let device = touchpad_clone.lock().unwrap();
                device.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING)
            })
            .await
            .unwrap();

            match result {
                Ok((_status, event)) => {
                    let decision = engine.handle_event(event);

                    for event in decision.forward_events {
                        if let Err(e) = virtual_touchpad.write_event(&event) {
                            warn!("Failed to forward touchpad event: {}", e);
                        }
                    }

                    if decision.touch_started {
                        activity_notifier.notify();
                    }

                    for action in decision.actions {
                        execute_touchpad_action(
                            action,
                            command_runner.clone(),
                            mpris_tracker.clone(),
                            tap_generation.clone(),
                            config.double_tap_seek_seconds,
                        );
                    }
                }
                Err(e) => {
                    if let Some(libc::ENODEV) = e.raw_os_error() {
                        info!("Touchpad {} disconnected. Exiting task.", path.display());
                        return;
                    }

                    warn!("Failed to read touchpad event: {:?}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

fn execute_touchpad_action(
    action: TouchpadAction,
    command_runner: UserCommandRunner,
    mpris_tracker: MprisTracker,
    tap_generation: Arc<AtomicU64>,
    double_tap_seek_seconds: u64,
) {
    match action {
        TouchpadAction::RunCommand(command) => {
            tokio::spawn(async move {
                match command_runner.run_shell_command(&command).await {
                    Ok(output) => {
                        if !output.success {
                            warn!(
                                "Touchpad command '{}' failed. stdout: {} stderr: {}",
                                command, output.stdout, output.stderr
                            );
                        }
                    }
                    Err(e) => warn!("{}", e),
                }
            });
        }
        TouchpadAction::TopSeek { fraction, speed } => {
            tokio::spawn(async move {
                mpris_tracker
                    .seek_by_fraction_and_speed(fraction, speed)
                    .await;
            });
        }
        TouchpadAction::ScheduleDoubleTap(corner) => {
            let generation = tap_generation.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                tokio::time::sleep(TAP_TIMEOUT).await;
                if tap_generation.load(Ordering::SeqCst) != generation {
                    return;
                }

                let seconds = double_tap_seek_seconds as f64;
                match corner {
                    Corner::Left => mpris_tracker.seek_by_seconds(-seconds).await,
                    Corner::Right => mpris_tracker.seek_by_seconds(seconds).await,
                }
            });
        }
        TouchpadAction::TripleTap(corner) => {
            tap_generation.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                match corner {
                    Corner::Left => mpris_tracker.previous().await,
                    Corner::Right => mpris_tracker.next().await,
                }
            });
        }
    }
}

#[derive(Clone, Copy)]
struct TouchpadGeometry {
    x: AxisRange,
    y: AxisRange,
}

impl TouchpadGeometry {
    fn from_device(device: &Device) -> Option<Self> {
        Some(Self {
            x: AxisRange::from_device(
                device,
                EventCode::EV_ABS(EV_ABS::ABS_X),
                EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_X),
            )?,
            y: AxisRange::from_device(
                device,
                EventCode::EV_ABS(EV_ABS::ABS_Y),
                EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_Y),
            )?,
        })
    }
}

#[derive(Clone, Copy)]
struct AxisRange {
    minimum: i32,
    maximum: i32,
}

impl AxisRange {
    fn from_device(device: &Device, primary: EventCode, fallback: EventCode) -> Option<Self> {
        device
            .abs_info(&primary)
            .or_else(|| device.abs_info(&fallback))
            .and_then(|info| {
                (info.maximum > info.minimum).then_some(Self {
                    minimum: info.minimum,
                    maximum: info.maximum,
                })
            })
    }

    fn normalize(&self, value: i32) -> f64 {
        ((value - self.minimum) as f64 / (self.maximum - self.minimum) as f64).clamp(0.0, 1.0)
    }
}

struct GestureEngine {
    config: TouchpadConfig,
    geometry: TouchpadGeometry,
    current_point: Option<TouchPoint>,
    active_gesture: Option<ActiveGesture>,
    pending_events: Vec<InputEvent>,
    touch_is_down: bool,
    packet_touch_started: bool,
    packet_touch_ended: bool,
    tap_tracker: TapTracker,
}

impl GestureEngine {
    fn new(config: TouchpadConfig, geometry: TouchpadGeometry) -> Self {
        Self {
            config,
            geometry,
            current_point: None,
            active_gesture: None,
            pending_events: Vec::new(),
            touch_is_down: false,
            packet_touch_started: false,
            packet_touch_ended: false,
            tap_tracker: TapTracker::default(),
        }
    }

    fn handle_event(&mut self, event: InputEvent) -> TouchpadDecision {
        self.observe_event(&event);
        self.pending_events.push(event.clone());

        if event.event_code == EventCode::EV_SYN(EV_SYN::SYN_REPORT) {
            self.handle_packet()
        } else {
            TouchpadDecision::default()
        }
    }

    fn observe_event(&mut self, event: &InputEvent) {
        match event.event_code {
            EventCode::EV_ABS(EV_ABS::ABS_X) | EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_X) => {
                let y = self.current_point.map(|point| point.y).unwrap_or(0.0);
                self.current_point = Some(TouchPoint {
                    x: self.geometry.x.normalize(event.value),
                    y,
                });
            }
            EventCode::EV_ABS(EV_ABS::ABS_Y) | EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_Y) => {
                let x = self.current_point.map(|point| point.x).unwrap_or(0.0);
                self.current_point = Some(TouchPoint {
                    x,
                    y: self.geometry.y.normalize(event.value),
                });
            }
            EventCode::EV_KEY(EV_KEY::BTN_TOUCH) => {
                if event.value != 0 && !self.touch_is_down {
                    self.packet_touch_started = true;
                    self.touch_is_down = true;
                } else if event.value == 0 && self.touch_is_down {
                    self.packet_touch_ended = true;
                    self.touch_is_down = false;
                }
            }
            _ => {}
        }
    }

    fn handle_packet(&mut self) -> TouchpadDecision {
        let mut decision = TouchpadDecision {
            touch_started: self.packet_touch_started,
            ..TouchpadDecision::default()
        };

        if self.packet_touch_started
            || (self.active_gesture.is_none() && self.touch_is_down && self.current_point.is_some())
        {
            if let Some(point) = self.current_point {
                let zone = classify_zone(
                    point,
                    self.config.left_border_percent,
                    self.config.right_border_percent,
                    self.config.top_border_percent,
                );
                self.active_gesture = Some(ActiveGesture::new(zone, point));
                debug!("Touchpad gesture started in {:?}", zone);
            }
        }

        if let Some(gesture) = self.active_gesture.as_mut() {
            if let Some(point) = self.current_point {
                decision.actions.extend(gesture.update(point, &self.config));
            }
        }

        let active_zone = self.active_gesture.as_ref().map(|gesture| gesture.zone);

        if self.packet_touch_ended {
            if let Some(gesture) = self.active_gesture.take() {
                decision
                    .actions
                    .extend(gesture.finish(&mut self.tap_tracker));
            }
        }

        if let Some(active_zone) = active_zone {
            if active_zone == TouchZone::Normal {
                decision.forward_events = std::mem::take(&mut self.pending_events);
            } else {
                self.pending_events.clear();
            }
        } else {
            decision.forward_events = std::mem::take(&mut self.pending_events);
        }

        self.packet_touch_started = false;
        self.packet_touch_ended = false;
        decision
    }
}

#[derive(Default)]
struct TouchpadDecision {
    forward_events: Vec<InputEvent>,
    actions: Vec<TouchpadAction>,
    touch_started: bool,
}

struct ActiveGesture {
    zone: TouchZone,
    start: TouchPoint,
    last: TouchPoint,
    started_at: Instant,
    last_at: Instant,
    vertical_remainder_percent: f64,
}

impl ActiveGesture {
    fn new(zone: TouchZone, start: TouchPoint) -> Self {
        let now = Instant::now();
        Self {
            zone,
            start,
            last: start,
            started_at: now,
            last_at: now,
            vertical_remainder_percent: 0.0,
        }
    }

    fn update(&mut self, point: TouchPoint, config: &TouchpadConfig) -> Vec<TouchpadAction> {
        let now = Instant::now();
        let mut actions = Vec::new();

        match self.zone {
            TouchZone::LeftBorder => {
                actions.extend(self.vertical_actions(
                    point,
                    config.brightness_step_percent,
                    &config.brightness_increment_command,
                    &config.brightness_decrement_command,
                ));
            }
            TouchZone::RightBorder => {
                actions.extend(self.vertical_actions(
                    point,
                    config.volume_step_percent,
                    &config.volume_increment_command,
                    &config.volume_decrement_command,
                ));
            }
            TouchZone::TopBorder => {
                actions.extend(self.top_seek_actions(point, now));
            }
            _ => {}
        }

        self.last = point;
        self.last_at = now;
        actions
    }

    fn finish(&self, tap_tracker: &mut TapTracker) -> Vec<TouchpadAction> {
        match self.zone {
            TouchZone::LeftCorner | TouchZone::RightCorner if self.is_tap() => {
                tap_tracker.register(self.zone.corner().unwrap())
            }
            _ => Vec::new(),
        }
    }

    fn vertical_actions(
        &mut self,
        point: TouchPoint,
        step_percent: f64,
        increment_command: &str,
        decrement_command: &str,
    ) -> Vec<TouchpadAction> {
        if step_percent <= 0.0 {
            return Vec::new();
        }

        let delta_percent = (self.last.y - point.y) * 100.0;
        self.vertical_remainder_percent += delta_percent;

        let mut actions = Vec::new();
        while self.vertical_remainder_percent + STEP_EPSILON >= step_percent {
            actions.push(TouchpadAction::RunCommand(increment_command.to_string()));
            self.vertical_remainder_percent -= step_percent;
        }

        while self.vertical_remainder_percent - STEP_EPSILON <= -step_percent {
            actions.push(TouchpadAction::RunCommand(decrement_command.to_string()));
            self.vertical_remainder_percent += step_percent;
        }

        actions
    }

    fn top_seek_actions(&self, point: TouchPoint, now: Instant) -> Vec<TouchpadAction> {
        if self.would_be_tap(point) {
            return Vec::new();
        }

        let fraction = point.x - self.last.x;
        if fraction.abs() < STEP_EPSILON {
            return Vec::new();
        }

        let elapsed = now.duration_since(self.last_at).as_secs_f64().max(0.001);
        let speed = fraction / elapsed;
        vec![TouchpadAction::TopSeek { fraction, speed }]
    }

    fn is_tap(&self) -> bool {
        self.would_be_tap(self.last)
    }

    fn would_be_tap(&self, point: TouchPoint) -> bool {
        self.started_at.elapsed() <= TAP_TIMEOUT
            && (point.x - self.start.x).abs() <= TAP_MOVEMENT_THRESHOLD
            && (point.y - self.start.y).abs() <= TAP_MOVEMENT_THRESHOLD
    }
}

#[derive(Default)]
struct TapTracker {
    last_corner: Option<Corner>,
    last_tap_at: Option<Instant>,
    count: u8,
}

impl TapTracker {
    fn register(&mut self, corner: Corner) -> Vec<TouchpadAction> {
        let now = Instant::now();
        if self.last_corner == Some(corner)
            && self
                .last_tap_at
                .is_some_and(|last_tap_at| now.duration_since(last_tap_at) <= TAP_TIMEOUT)
        {
            self.count += 1;
        } else {
            self.count = 1;
        }

        self.last_corner = Some(corner);
        self.last_tap_at = Some(now);

        match self.count {
            2 => vec![TouchpadAction::ScheduleDoubleTap(corner)],
            3.. => {
                self.count = 0;
                vec![TouchpadAction::TripleTap(corner)]
            }
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TouchPoint {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TouchZone {
    Normal,
    LeftBorder,
    RightBorder,
    TopBorder,
    LeftCorner,
    RightCorner,
}

impl TouchZone {
    fn corner(self) -> Option<Corner> {
        match self {
            TouchZone::LeftCorner => Some(Corner::Left),
            TouchZone::RightCorner => Some(Corner::Right),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Corner {
    Left,
    Right,
}

enum TouchpadAction {
    RunCommand(String),
    TopSeek { fraction: f64, speed: f64 },
    ScheduleDoubleTap(Corner),
    TripleTap(Corner),
}

fn classify_zone(
    point: TouchPoint,
    left_border_percent: f64,
    right_border_percent: f64,
    top_border_percent: f64,
) -> TouchZone {
    let left_border = percent_to_fraction(left_border_percent);
    let right_border = percent_to_fraction(right_border_percent);
    let top_border = percent_to_fraction(top_border_percent);

    if point.y <= top_border {
        if point.x <= left_border {
            TouchZone::LeftCorner
        } else if point.x >= 1.0 - right_border {
            TouchZone::RightCorner
        } else {
            TouchZone::TopBorder
        }
    } else if point.x <= left_border {
        TouchZone::LeftBorder
    } else if point.x >= 1.0 - right_border {
        TouchZone::RightBorder
    } else {
        TouchZone::Normal
    }
}

fn percent_to_fraction(percent: f64) -> f64 {
    (percent / 100.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_overlapping_top_corners_before_borders() {
        assert_eq!(
            classify_zone(TouchPoint { x: 0.05, y: 0.05 }, 10.0, 10.0, 12.0),
            TouchZone::LeftCorner
        );
        assert_eq!(
            classify_zone(TouchPoint { x: 0.95, y: 0.05 }, 10.0, 10.0, 12.0),
            TouchZone::RightCorner
        );
    }

    #[test]
    fn classifies_left_right_top_and_normal_zones() {
        assert_eq!(
            classify_zone(TouchPoint { x: 0.05, y: 0.5 }, 10.0, 10.0, 12.0),
            TouchZone::LeftBorder
        );
        assert_eq!(
            classify_zone(TouchPoint { x: 0.95, y: 0.5 }, 10.0, 10.0, 12.0),
            TouchZone::RightBorder
        );
        assert_eq!(
            classify_zone(TouchPoint { x: 0.5, y: 0.05 }, 10.0, 10.0, 12.0),
            TouchZone::TopBorder
        );
        assert_eq!(
            classify_zone(TouchPoint { x: 0.5, y: 0.5 }, 10.0, 10.0, 12.0),
            TouchZone::Normal
        );
    }

    #[test]
    fn vertical_movement_accumulates_step_actions() {
        let config = TouchpadConfig::default();
        let mut gesture = ActiveGesture::new(TouchZone::LeftBorder, TouchPoint { x: 0.05, y: 1.0 });

        let actions = gesture.update(TouchPoint { x: 0.05, y: 0.9 }, &config);
        assert_eq!(actions.len(), 2);
    }
}
