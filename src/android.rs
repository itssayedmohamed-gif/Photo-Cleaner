//! Android front end: egui UI (glow/EGL on a NativeActivity) + JNI bridge to
//! MediaStore. Compiled only for `target_os = "android"`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use android_activity::AndroidApp;
use eframe::egui::{
    self, load::SizedTexture, pos2, vec2, Align2, Color32, FontId, Id, Rect, Sense, Stroke,
    TextStyle, TextureHandle, TextureOptions,
};

use crate::store::{Decision, Store};

// ---------------------------------------------------------------- theme ----
const BG: Color32 = Color32::from_rgb(0x11, 0x13, 0x18);
const SURFACE: Color32 = Color32::from_rgb(0x1D, 0x20, 0x25);
const SURFACE_HI: Color32 = Color32::from_rgb(0x2B, 0x2F, 0x36);
const PRIMARY: Color32 = Color32::from_rgb(0xA8, 0xC7, 0xFA);
const ON_PRIMARY: Color32 = Color32::from_rgb(0x06, 0x2E, 0x6F);
const TEXT: Color32 = Color32::from_rgb(0xE3, 0xE2, 0xE6);
const TEXT_DIM: Color32 = Color32::from_rgb(0xA9, 0xAC, 0xB6);
const KEEP: Color32 = Color32::from_rgb(0x81, 0xC9, 0x95);
const DEL: Color32 = Color32::from_rgb(0xF2, 0xB8, 0xB5);
const DEL_BG: Color32 = Color32::from_rgb(0x5C, 0x1F, 0x1C);

const MAX_BATCH: usize = 200;

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE_HI;
    v.window_rounding = 28.0.into();
    v.window_stroke = Stroke::NONE;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = PRIMARY;
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = 22.0.into();
        w.weak_bg_fill = SURFACE;
        w.bg_fill = SURFACE;
    }
    ctx.style_mut(|s| {
        s.visuals = v;
        s.text_styles.insert(TextStyle::Body, FontId::proportional(17.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(17.0));
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(26.0));
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(18.0, 12.0);
    });
}

// ------------------------------------------------------------ JNI bridge ----
mod bridge {
    use jni::objects::{JByteArray, JLongArray, JObject, JValue};
    use jni::sys::jboolean;
    use jni::{JNIEnv, JavaVM};
    use std::sync::Mutex;

    pub enum Event {
        Delete(bool),
        Permission(bool),
    }

    static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
    pub static WAKE: Mutex<Option<eframe::egui::Context>> = Mutex::new(None);

    fn push(e: Event) {
        EVENTS.lock().unwrap_or_else(|p| p.into_inner()).push(e);
        if let Some(c) = WAKE.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            c.request_repaint();
        }
    }

    pub fn take_events() -> Vec<Event> {
        std::mem::take(&mut *EVENTS.lock().unwrap_or_else(|p| p.into_inner()))
    }

    #[no_mangle]
    #[allow(non_snake_case)]
    pub extern "system" fn Java_dev_photoclean_app_MainActivity_nativeOnDeleteResult(
        _env: JNIEnv,
        _this: JObject,
        ok: jboolean,
    ) {
        push(Event::Delete(ok != 0));
    }

    #[no_mangle]
    #[allow(non_snake_case)]
    pub extern "system" fn Java_dev_photoclean_app_MainActivity_nativeOnPermissionResult(
        _env: JNIEnv,
        _this: JObject,
        granted: jboolean,
    ) {
        push(Event::Permission(granted != 0));
    }

    pub fn vm() -> JavaVM {
        let c = ndk_context::android_context();
        unsafe { JavaVM::from_raw(c.vm().cast()) }.expect("JavaVM")
    }

    /// The Activity (a global ref owned by android-activity; never deleted here).
    pub fn activity() -> JObject<'static> {
        unsafe { JObject::from_raw(ndk_context::android_context().context().cast()) }
    }

    pub fn has_permission() -> bool {
        let vm = vm();
        let Ok(mut env) = vm.attach_current_thread() else { return false };
        let act = activity();
        let r = env
            .call_method(&act, "hasPermission", "()Z", &[])
            .and_then(|v| v.z())
            .unwrap_or(false);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        r
    }

    fn call_void(name: &str) {
        let vm = vm();
        let Ok(mut env) = vm.attach_current_thread() else { return };
        let act = activity();
        if env.call_method(&act, name, "()V", &[]).is_err() {
            let _ = env.exception_clear();
        }
    }

    pub fn request_permission() {
        call_void("requestMediaPermission");
    }

    pub fn open_app_settings() {
        call_void("openAppSettings");
    }

    /// Ask Android to delete these MediaStore image ids (system confirmation dialog).
    pub fn request_delete(ids: &[i64]) -> bool {
        let vm = vm();
        let Ok(mut env) = vm.attach_current_thread() else { return false };
        let act = activity();
        let Ok(arr) = env.new_long_array(ids.len() as i32) else { return false };
        if env.set_long_array_region(&arr, 0, ids).is_err() {
            return false;
        }
        let ok = env
            .call_method(&act, "requestDelete", "([J)V", &[JValue::Object(&arr)])
            .is_ok();
        if !ok {
            let _ = env.exception_clear();
        }
        let _ = env.delete_local_ref(arr);
        ok
    }

    // ---- worker-thread helpers (called with an already attached env) ----
    pub fn query_ids(env: &mut JNIEnv, act: &JObject) -> Vec<i64> {
        let obj = match env.call_method(act, "queryIds", "()[J", &[]).and_then(|v| v.l()) {
            Ok(o) if !o.is_null() => o,
            _ => {
                let _ = env.exception_clear();
                return Vec::new();
            }
        };
        let arr = JLongArray::from(obj);
        let n = env.get_array_length(&arr).unwrap_or(0).max(0) as usize;
        let mut out = vec![0i64; n];
        if env.get_long_array_region(&arr, 0, &mut out).is_err() {
            out.clear();
        }
        let _ = env.delete_local_ref(arr);
        out
    }

    pub fn load_thumb(
        env: &mut JNIEnv,
        act: &JObject,
        id: i64,
        w: i32,
        h: i32,
    ) -> Option<eframe::egui::ColorImage> {
        let obj = match env
            .call_method(act, "loadThumb", "(JII)[B", &[JValue::Long(id), JValue::Int(w), JValue::Int(h)])
            .and_then(|v| v.l())
        {
            Ok(o) if !o.is_null() => o,
            _ => {
                let _ = env.exception_clear();
                return None;
            }
        };
        let arr = JByteArray::from(obj);
        let bytes = env.convert_byte_array(&arr).ok();
        let _ = env.delete_local_ref(arr);
        let bytes = bytes?;
        if bytes.len() < 8 {
            return None;
        }
        let bw = i32::from_le_bytes(bytes[0..4].try_into().ok()?).max(0) as usize;
        let bh = i32::from_le_bytes(bytes[4..8].try_into().ok()?).max(0) as usize;
        if bw == 0 || bh == 0 || bytes.len() < 8 + bw * bh * 4 {
            return None;
        }
        Some(eframe::egui::ColorImage::from_rgba_premultiplied(
            [bw, bh],
            &bytes[8..8 + bw * bh * 4],
        ))
    }
}

// ---------------------------------------------------------------- worker ----
enum Req {
    Ids,
    Thumb { id: i64, w: i32, h: i32 },
}
enum Resp {
    Ids(Vec<i64>),
    Thumb(i64, Option<egui::ColorImage>),
}

fn spawn_worker(ctx: egui::Context) -> (Sender<Req>, Receiver<Resp>) {
    let (req_tx, req_rx) = channel::<Req>();
    let (resp_tx, resp_rx) = channel::<Resp>();
    std::thread::Builder::new()
        .name("media-worker".into())
        .spawn(move || {
            let vm = bridge::vm();
            let Ok(mut env) = vm.attach_current_thread() else { return };
            let act = bridge::activity();
            while let Ok(req) = req_rx.recv() {
                let resp = match req {
                    Req::Ids => Resp::Ids(bridge::query_ids(&mut env, &act)),
                    Req::Thumb { id, w, h } => Resp::Thumb(id, bridge::load_thumb(&mut env, &act, id, w, h)),
                };
                if resp_tx.send(resp).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        })
        .expect("spawn media worker");
    (req_tx, resp_rx)
}

// ------------------------------------------------------------------- app ----
#[derive(Clone, Copy, PartialEq)]
enum Perm {
    Granted,
    Missing,
}

#[derive(Clone, Copy)]
struct Fly {
    id: i64,
    dir: f32,
    from: f32,
    t0: f64,
}

struct App {
    store: Store,
    ids: Vec<i64>,
    index_of: HashMap<i64, usize>,
    cursor: usize, // first undecided position == number reviewed
    perm: Perm,
    loaded: bool,
    ids_requested: bool,
    tex: HashMap<i64, TextureHandle>,
    requested: HashSet<i64>,
    failed: HashSet<i64>,
    tx: Sender<Req>,
    rx: Receiver<Resp>,
    offset: f32,
    fly: Option<Fly>,
    confirm: bool,
    deleting: Option<Vec<i64>>,
    toast: Option<(String, f64)>,
    last_perm_poll: f64,
}

pub fn run(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("photocleaner"),
    );
    let data_dir = app.internal_data_path();
    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };
    let res = eframe::run_native(
        "Photo Cleaner",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, data_dir)))),
    );
    if let Err(e) = res {
        log::error!("eframe exited with error: {e}");
    }
}

fn fit(tsize: egui::Vec2, card: Rect) -> egui::Vec2 {
    tsize * (card.width() / tsize.x).min(card.height() / tsize.y)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, data_dir: Option<PathBuf>) -> Self {
        apply_theme(&cc.egui_ctx);
        *bridge::WAKE.lock().unwrap_or_else(|p| p.into_inner()) = Some(cc.egui_ctx.clone());
        let path = data_dir.unwrap_or_else(std::env::temp_dir).join("decisions.log");
        let (tx, rx) = spawn_worker(cc.egui_ctx.clone());
        App {
            store: Store::open(&path),
            ids: Vec::new(),
            index_of: HashMap::new(),
            cursor: 0,
            perm: if bridge::has_permission() { Perm::Granted } else { Perm::Missing },
            loaded: false,
            ids_requested: false,
            tex: HashMap::new(),
            requested: HashSet::new(),
            failed: HashSet::new(),
            tx,
            rx,
            offset: 0.0,
            fly: None,
            confirm: false,
            deleting: None,
            toast: None,
            last_perm_poll: 0.0,
        }
    }

    fn show_toast(&mut self, ctx: &egui::Context, msg: impl Into<String>) {
        self.toast = Some((msg.into(), ctx.input(|i| i.time)));
    }

    fn rebuild_index(&mut self) {
        self.index_of = self.ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    }

    fn advance(&mut self) {
        while self.cursor < self.ids.len() && self.store.is_decided(self.ids[self.cursor]) {
            self.cursor += 1;
        }
    }

    fn reset_cursor(&mut self) {
        self.cursor = 0;
        self.advance();
    }

    fn on_ids(&mut self, v: Vec<i64>) {
        let present: HashSet<i64> = v.iter().copied().collect();
        self.store.prune_missing(&present);
        self.ids = v;
        self.rebuild_index();
        self.reset_cursor();
        self.loaded = true;
        self.ids_requested = false;
    }

    fn decide(&mut self, id: i64, keep: bool) {
        self.store.record(if keep { Decision::Keep } else { Decision::Delete }, id);
        self.advance();
    }

    fn undo(&mut self) {
        if let Some(id) = self.store.undo() {
            match self.index_of.get(&id) {
                Some(&i) => self.cursor = i,
                None => self.reset_cursor(),
            }
            self.offset = 0.0;
        }
    }

    fn start_fly(&mut self, ctx: &egui::Context, dir: f32) {
        if let Some(&id) = self.ids.get(self.cursor) {
            if self.fly.is_none() {
                self.fly = Some(Fly { id, dir, from: self.offset, t0: ctx.input(|i| i.time) });
            }
        }
    }

    fn start_delete(&mut self, ctx: &egui::Context) {
        self.confirm = false;
        let batch = self.store.pending_batch(MAX_BATCH);
        if batch.is_empty() {
            return;
        }
        if bridge::request_delete(&batch) {
            self.deleting = Some(batch);
        } else {
            self.show_toast(ctx, "Couldn't start the delete request");
        }
    }

    fn on_delete_result(&mut self, ctx: &egui::Context, ok: bool) {
        let Some(batch) = self.deleting.take() else { return };
        if ok {
            self.store.forget(&batch);
            let gone: HashSet<i64> = batch.iter().copied().collect();
            self.ids.retain(|id| !gone.contains(id));
            self.rebuild_index();
            self.reset_cursor();
            for id in &gone {
                self.tex.remove(id);
            }
            self.show_toast(ctx, format!("Deleted {} photo{}", gone.len(), if gone.len() == 1 { "" } else { "s" }));
        } else {
            self.show_toast(ctx, "Deletion cancelled. Photos are still marked.");
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        for ev in bridge::take_events() {
            match ev {
                bridge::Event::Permission(ok) => {
                    self.perm = if ok { Perm::Granted } else { Perm::Missing };
                }
                bridge::Event::Delete(ok) => self.on_delete_result(ctx, ok),
            }
        }
        if self.perm == Perm::Missing {
            // Cheap poll so returning from system Settings picks up the grant.
            if now - self.last_perm_poll > 0.5 {
                self.last_perm_poll = now;
                if bridge::has_permission() {
                    self.perm = Perm::Granted;
                }
            }
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if self.perm == Perm::Granted && !self.loaded && !self.ids_requested {
            self.ids_requested = true;
            let _ = self.tx.send(Req::Ids);
        }
        while let Ok(r) = self.rx.try_recv() {
            match r {
                Resp::Ids(v) => self.on_ids(v),
                Resp::Thumb(id, img) => {
                    self.requested.remove(&id);
                    match img {
                        Some(ci) => {
                            let t = ctx.load_texture(format!("photo-{id}"), ci, TextureOptions::LINEAR);
                            self.tex.insert(id, t);
                        }
                        None => {
                            self.failed.insert(id);
                        }
                    }
                }
            }
        }
        self.want(ctx);
    }

    /// Keep only current + next two + previous photo in memory; prefetch those.
    fn want(&mut self, ctx: &egui::Context) {
        if !self.loaded {
            return;
        }
        let mut wanted: Vec<i64> = Vec::with_capacity(4);
        let mut i = self.cursor;
        while wanted.len() < 3 && i < self.ids.len() {
            wanted.push(self.ids[i]);
            i += 1;
        }
        if self.cursor > 0 && self.cursor <= self.ids.len() {
            wanted.push(self.ids[self.cursor - 1]);
        }
        self.tex.retain(|id, _| wanted.contains(id));
        self.failed.retain(|id| wanted.contains(id));
        let px = ctx.screen_rect().size() * ctx.pixels_per_point();
        let w = (px.x as i32).clamp(256, 1600);
        let h = ((px.y * 0.8) as i32).clamp(256, 2400);
        for id in wanted {
            if !self.tex.contains_key(&id) && !self.requested.contains(&id) && !self.failed.contains(&id) {
                self.requested.insert(id);
                let _ = self.tx.send(Req::Thumb { id, w, h });
            }
        }
    }
}

// -------------------------------------------------------------- widgets ----
fn pill(ui: &mut egui::Ui, id: &str, rect: Rect, text: &str, fill: Color32, fg: Color32, enabled: bool) -> bool {
    let resp = ui.interact(rect, Id::new(id), if enabled { Sense::click() } else { Sense::hover() });
    let mut f = fill;
    if enabled && resp.is_pointer_button_down_on() {
        f = f.gamma_multiply(0.75);
    }
    let a = if enabled { 1.0 } else { 0.35 };
    ui.painter().rect_filled(rect, rect.height() / 2.0, f.gamma_multiply(a));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(16.0),
        fg.gamma_multiply(a),
    );
    enabled && resp.clicked()
}

fn round_button(ui: &mut egui::Ui, id: &str, c: egui::Pos2, keep: bool, enabled: bool) -> bool {
    let rect = Rect::from_center_size(c, vec2(68.0, 68.0));
    let resp = ui.interact(rect, Id::new(id), if enabled { Sense::click() } else { Sense::hover() });
    let (fill, fg) = if keep { (Color32::from_rgb(0x1E, 0x4D, 0x2B), KEEP) } else { (DEL_BG, DEL) };
    let a = if enabled { 1.0 } else { 0.35 };
    let mut f = fill;
    if enabled && resp.is_pointer_button_down_on() {
        f = f.gamma_multiply(0.7);
    }
    let p = ui.painter();
    p.circle_filled(c, 32.0, f.gamma_multiply(a));
    let s = Stroke::new(3.5, fg.gamma_multiply(a));
    if keep {
        p.line_segment([c + vec2(-10.0, 0.0), c + vec2(-3.0, 8.0)], s);
        p.line_segment([c + vec2(-3.0, 8.0), c + vec2(11.0, -8.0)], s);
    } else {
        p.line_segment([c + vec2(-9.0, -9.0), c + vec2(9.0, 9.0)], s);
        p.line_segment([c + vec2(-9.0, 9.0), c + vec2(9.0, -9.0)], s);
    }
    enabled && resp.clicked()
}

// ------------------------------------------------------------------- UI ----
impl App {
    fn busy(&self) -> bool {
        self.fly.is_some() || self.deleting.is_some() || self.confirm
    }

    fn ui_root(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let p = ui.painter().clone();
        if self.perm == Perm::Missing {
            p.text(full.center() + vec2(0.0, -90.0), Align2::CENTER_CENTER, "Photo access needed", FontId::proportional(24.0), TEXT);
            p.text(
                full.center() + vec2(0.0, -48.0),
                Align2::CENTER_CENTER,
                "Allow access so photos can be reviewed.\nNothing leaves your device.",
                FontId::proportional(15.0),
                TEXT_DIM,
            );
            let r1 = Rect::from_center_size(full.center() + vec2(0.0, 20.0), vec2(220.0, 48.0));
            if pill(ui, "grant", r1, "Allow photo access", PRIMARY, ON_PRIMARY, true) {
                bridge::request_permission();
            }
            let r2 = Rect::from_center_size(full.center() + vec2(0.0, 82.0), vec2(220.0, 44.0));
            if pill(ui, "settings", r2, "Open app settings", SURFACE_HI, TEXT, true) {
                bridge::open_app_settings();
            }
            return;
        }
        if !self.loaded {
            ui.put(Rect::from_center_size(full.center(), vec2(36.0, 36.0)), egui::Spinner::new());
            return;
        }
        if self.ids.is_empty() {
            p.text(full.center(), Align2::CENTER_CENTER, "No photos found", FontId::proportional(22.0), TEXT_DIM);
            return;
        }
        self.ui_header(ui, full);
        if self.cursor >= self.ids.len() {
            self.ui_done(ctx, ui, full);
        } else {
            self.ui_card(ctx, ui, full);
            self.ui_controls(ctx, ui, full);
        }
    }

    fn ui_header(&mut self, ui: &mut egui::Ui, full: Rect) {
        let head = Rect::from_min_size(full.min + vec2(20.0, 10.0), vec2(full.width() - 40.0, 44.0));
        let total = self.ids.len();
        ui.painter().text(
            head.left_center(),
            Align2::LEFT_CENTER,
            format!("{} of {}", self.cursor, total),
            FontId::proportional(18.0),
            TEXT,
        );
        let n = self.store.pending().len();
        if n > 0 {
            let r = Rect::from_center_size(head.right_center() - vec2(56.0, 0.0), vec2(112.0, 36.0));
            if pill(ui, "chip", r, &format!("Delete {n}"), DEL_BG, DEL, !self.busy()) {
                self.confirm = true;
            }
        }
        let bar = Rect::from_min_size(pos2(head.left(), head.bottom() + 6.0), vec2(head.width(), 4.0));
        ui.painter().rect_filled(bar, 2.0, SURFACE_HI);
        let frac = if total == 0 { 0.0 } else { self.cursor as f32 / total as f32 };
        ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * frac, 4.0)), 2.0, PRIMARY);
    }

    fn ui_done(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, full: Rect) {
        let p = ui.painter().clone();
        p.text(full.center() + vec2(0.0, -60.0), Align2::CENTER_CENTER, "All caught up", FontId::proportional(26.0), TEXT);
        let n = self.store.pending().len();
        p.text(
            full.center() + vec2(0.0, -22.0),
            Align2::CENTER_CENTER,
            if n > 0 { format!("{n} marked for deletion") } else { "Nothing marked for deletion".to_string() },
            FontId::proportional(15.0),
            TEXT_DIM,
        );
        if n > 0 {
            let r = Rect::from_center_size(full.center() + vec2(0.0, 40.0), vec2(230.0, 48.0));
            if pill(ui, "review", r, &format!("Review & delete {n}"), DEL_BG, DEL, !self.busy()) {
                self.confirm = true;
            }
        }
        let u = Rect::from_center_size(pos2(full.center().x, full.bottom() - 64.0), vec2(96.0, 44.0));
        if pill(ui, "undo", u, "Undo", SURFACE_HI, TEXT, !self.busy() && self.store.can_undo()) {
            self.undo();
        }
        let _ = ctx;
    }

    fn ui_card(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, full: Rect) {
        let card = Rect::from_min_max(
            pos2(full.left() + 16.0, full.top() + 76.0),
            pos2(full.right() - 16.0, full.bottom() - 132.0),
        );
        let busy = self.busy();
        let resp = ui.interact(card, Id::new("card"), if busy { Sense::hover() } else { Sense::drag() });
        let now = ctx.input(|i| i.time);

        if !busy {
            if resp.dragged() {
                self.offset += resp.drag_delta().x;
                ctx.request_repaint();
            }
            if resp.drag_stopped() {
                let vel = ctx.input(|i| i.pointer.velocity().x);
                let dir = self.offset.signum();
                if self.offset.abs() > card.width() * 0.3 || (vel * dir > 800.0 && self.offset.abs() > 16.0) {
                    self.start_fly(ctx, dir);
                }
            }
        }
        if let Some(f) = self.fly {
            let t = (((now - f.t0) / 0.2) as f32).clamp(0.0, 1.0);
            let e = 1.0 - (1.0 - t).powi(3);
            self.offset = f.from + (f.dir * full.width() * 1.2 - f.from) * e;
            ctx.request_repaint();
            if t >= 1.0 {
                self.fly = None;
                self.offset = 0.0;
                self.decide(f.id, f.dir > 0.0);
            }
        } else if !resp.dragged() && self.offset != 0.0 {
            let dt = ctx.input(|i| i.stable_dt).min(0.05);
            self.offset *= (-dt * 16.0).exp();
            if self.offset.abs() < 0.5 {
                self.offset = 0.0;
            }
            ctx.request_repaint();
        }

        let Some(&id) = self.ids.get(self.cursor) else { return };

        // Next photo peeks in behind while dragging.
        if self.offset.abs() > 2.0 {
            if let Some(t) = self.ids.get(self.cursor + 1).and_then(|n| self.tex.get(n)).cloned() {
                let k = (self.offset.abs() / (card.width() * 0.5)).min(1.0);
                let sz = fit(t.size_vec2(), card) * (0.92 + 0.08 * k);
                let r = Rect::from_center_size(card.center(), sz);
                ui.put(
                    r,
                    egui::Image::new(SizedTexture::new(t.id(), sz))
                        .rounding(24.0)
                        .tint(Color32::from_gray((110.0 + 110.0 * k) as u8)),
                );
            }
        }

        let shown = match self.tex.get(&id).cloned() {
            Some(t) => {
                let sz = fit(t.size_vec2(), card);
                let r = Rect::from_center_size(card.center() + vec2(self.offset, 0.0), sz);
                ui.put(r, egui::Image::new(SizedTexture::new(t.id(), sz)).rounding(24.0));
                r
            }
            None => {
                let r = card.translate(vec2(self.offset, 0.0));
                ui.painter().rect_filled(r, 24.0, SURFACE);
                if self.failed.contains(&id) {
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "Can't load this photo", FontId::proportional(16.0), TEXT_DIM);
                } else {
                    ui.put(Rect::from_center_size(r.center(), vec2(32.0, 32.0)), egui::Spinner::new());
                }
                r
            }
        };

        // Direction label that fades in with the drag.
        let a = (self.offset.abs() / (card.width() * 0.25)).min(1.0);
        if a > 0.02 {
            let right = self.offset < 0.0;
            let (label, col, w) = if right { ("DELETE", DEL, 108.0) } else { ("KEEP", KEEP, 84.0) };
            let rr = if right {
                Rect::from_min_size(shown.right_top() + vec2(-20.0 - w, 20.0), vec2(w, 38.0))
            } else {
                Rect::from_min_size(shown.left_top() + vec2(20.0, 20.0), vec2(w, 38.0))
            };
            ui.painter().rect_filled(rr, 19.0, Color32::from_black_alpha(150).gamma_multiply(a));
            ui.painter().text(rr.center(), Align2::CENTER_CENTER, label, FontId::proportional(16.0), col.gamma_multiply(a));
        }
    }

    fn ui_controls(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, full: Rect) {
        let y = full.bottom() - 62.0;
        let cx = full.center().x;
        let enabled = !self.busy();
        if round_button(ui, "btn-del", pos2(cx - 104.0, y), false, enabled) {
            self.start_fly(ctx, -1.0);
        }
        if round_button(ui, "btn-keep", pos2(cx + 104.0, y), true, enabled) {
            self.start_fly(ctx, 1.0);
        }
        let u = Rect::from_center_size(pos2(cx, y), vec2(88.0, 42.0));
        if pill(ui, "undo", u, "Undo", SURFACE_HI, TEXT, enabled && self.store.can_undo()) {
            self.undo();
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm {
            return;
        }
        let n = self.store.pending().len();
        let now_n = n.min(MAX_BATCH);
        let mut action = 0;
        egui::Window::new("Delete photos?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(290.0);
                ui.label(format!(
                    "{} photo{} marked for deletion will be permanently removed from this device.",
                    now_n,
                    if now_n == 1 { "" } else { "s" }
                ));
                if n > now_n {
                    ui.label(format!("{} more will be offered next time.", n - now_n));
                }
                ui.label("Android will ask you to confirm once more.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("Cancel").min_size(vec2(110.0, 44.0))).clicked() {
                        action = 1;
                    }
                    if ui
                        .add(egui::Button::new(egui::RichText::new("Delete").color(DEL)).min_size(vec2(110.0, 44.0)))
                        .clicked()
                    {
                        action = 2;
                    }
                });
            });
        match action {
            1 => self.confirm = false,
            2 => self.start_delete(ctx),
            _ => {}
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, t0)) = self.toast.clone() else { return };
        if ctx.input(|i| i.time) - t0 > 3.0 {
            self.toast = None;
            return;
        }
        egui::Area::new(Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -130.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(SURFACE_HI)
                    .rounding(18.0)
                    .inner_margin(egui::Margin::symmetric(18.0, 10.0))
                    .show(ui, |ui| ui.label(msg));
            });
        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG))
            .show(ctx, |ui| self.ui_root(ctx, ui));
        self.confirm_dialog(ctx);
        self.toast_ui(ctx);
    }
}

