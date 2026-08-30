#![cfg_attr(windows, windows_subsystem = "windows")]

use personalrag_v2::extraction::ExtractorConfig;
use std::path::PathBuf;

#[derive(Clone, Debug)]
enum GuiLaunchMode {
    ZeroConfig,
    Legacy { root: PathBuf, store: PathBuf },
}

#[derive(Clone, Debug)]
struct GuiArgs {
    mode: GuiLaunchMode,
    extractor: ExtractorConfig,
}

fn parse_args() -> Result<GuiArgs, String> {
    let mut root = std::env::var_os("PERSONALRAG_ROOT").map(PathBuf::from);
    let mut store = std::env::var_os("PERSONALRAG_STORE").map(PathBuf::from);
    let mut extractor = ExtractorConfig::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--root" => {
                root = Some(PathBuf::from(args.next().ok_or("--root requires a path")?));
            }
            "--store" => {
                store = Some(PathBuf::from(args.next().ok_or("--store requires a path")?));
            }
            "--pdftotext" => {
                extractor.pdftotext =
                    PathBuf::from(args.next().ok_or("--pdftotext requires a path")?);
            }
            "--unzip" => {
                extractor.unzip = PathBuf::from(args.next().ok_or("--unzip requires a path")?);
            }
            "--zstd" => {
                extractor.zstd = PathBuf::from(args.next().ok_or("--zstd requires a path")?);
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown argument: {other}\n\n{}", usage())),
        }
    }
    let mode = match (root, store) {
        (None, None) => GuiLaunchMode::ZeroConfig,
        (Some(root), Some(store)) => GuiLaunchMode::Legacy { root, store },
        _ => {
            return Err(
                "--root and --store must be supplied together; omit both for zero-config mode"
                    .to_string(),
            );
        }
    };
    Ok(GuiArgs { mode, extractor })
}

fn usage() -> String {
    concat!(
        "PersonalRag V2 GUI\n",
        "Usage: personalrag-v2-gui [--pdftotext <path>] [--unzip <path>] [--zstd <path>]\n",
        "       personalrag-v2-gui --root <indexed-root> --store <index-store> [helper overrides]\n",
        "With no root/store, PersonalRag discovers all fixed local drives, stores indexes under ",
        "%LOCALAPPDATA%\\PersonalRag, and indexes continuously in the background.\n",
        "PERSONALRAG_ROOT and PERSONALRAG_STORE remain available for legacy/developer mode."
    )
    .to_string()
}

#[cfg(not(windows))]
fn main() {
    match parse_args() {
        Ok(args) => {
            let mode = match &args.mode {
                GuiLaunchMode::ZeroConfig => "zero-config".to_string(),
                GuiLaunchMode::Legacy { root, store } => {
                    format!("legacy root={} store={}", root.display(), store.display())
                }
            };
            eprintln!(
                "PersonalRag V2 GUI is Windows-only. Parsed mode={} pdftotext={} unzip={} zstd={}",
                mode,
                args.extractor.pdftotext.display(),
                args.extractor.unzip.display(),
                args.extractor.zstd.display()
            );
        }
        Err(error) => eprintln!("{error}"),
    }
}

#[cfg(windows)]
fn main() {
    let result = parse_args().and_then(windows_ui::run);
    if let Err(error) = result {
        windows_ui::show_error("PersonalRag V2", &error);
    }
}

#[cfg(windows)]
mod windows_ui {
    use super::{GuiArgs, GuiLaunchMode};
    use personalrag_v2::app::{AppRuntimeHandle, RuntimeReader, RuntimeSnapshot};
    use personalrag_v2::gui::{
        GuiContentMode, GuiFileScope, GuiIndexStatus, GuiResultRow, GuiSearchRequest,
        GuiSearchResponse, GuiSearchSession, format_file_size, format_modified_ns_utc,
    };
    use personalrag_v2::gui_app::AppGuiSearchSession;
    use std::ffi::c_void;
    use std::mem;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;

    type Bool = i32;
    type Uint = u32;
    type Dword = u32;
    type Wparam = usize;
    type Lparam = isize;
    type Lresult = isize;
    type Hwnd = *mut c_void;
    type Hinstance = *mut c_void;
    type Hicon = *mut c_void;
    type Hcursor = *mut c_void;
    type Hbrush = *mut c_void;
    type Hmenu = *mut c_void;
    type Hfont = *mut c_void;

    const TRUE: Bool = 1;
    const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
    const SW_SHOW: i32 = 5;
    const COLOR_WINDOW: isize = 5;
    const IDC_ARROW: usize = 32512;
    const DEFAULT_GUI_FONT: i32 = 17;
    const GWLP_USERDATA: i32 = -21;

    const WS_OVERLAPPEDWINDOW: Dword = 0x00cf_0000;
    const WS_CHILD: Dword = 0x4000_0000;
    const WS_VISIBLE: Dword = 0x1000_0000;
    const WS_TABSTOP: Dword = 0x0001_0000;
    const WS_BORDER: Dword = 0x0080_0000;
    const WS_VSCROLL: Dword = 0x0020_0000;
    const ES_AUTOHSCROLL: Dword = 0x0080;
    const ES_MULTILINE: Dword = 0x0004;
    const ES_AUTOVSCROLL: Dword = 0x0040;
    const ES_READONLY: Dword = 0x0800;
    const BS_PUSHBUTTON: Dword = 0;
    const BS_AUTOCHECKBOX: Dword = 0x0003;
    const CBS_DROPDOWNLIST: Dword = 0x0003;
    const LVS_REPORT: Dword = 0x0001;
    const LVS_SINGLESEL: Dword = 0x0004;
    const LVS_SHOWSELALWAYS: Dword = 0x0008;
    const LVS_EX_GRIDLINES: Wparam = 0x0000_0001;
    const LVS_EX_FULLROWSELECT: Wparam = 0x0000_0020;
    const LVS_EX_DOUBLEBUFFER: Wparam = 0x0001_0000;

    const WM_CREATE: Uint = 0x0001;
    const WM_DESTROY: Uint = 0x0002;
    const WM_SIZE: Uint = 0x0005;
    const WM_SETFOCUS: Uint = 0x0007;
    const WM_COMMAND: Uint = 0x0111;
    const WM_TIMER: Uint = 0x0113;
    const WM_NOTIFY: Uint = 0x004e;
    const WM_NCDESTROY: Uint = 0x0082;
    const WM_SETFONT: Uint = 0x0030;
    const WM_APP_SEARCH_DONE: Uint = 0x8001;
    const WM_APP_RELOAD_DONE: Uint = 0x8002;

    const EN_CHANGE: u16 = 0x0300;
    const BN_CLICKED: u16 = 0;
    const CBN_SELCHANGE: u16 = 1;
    const BM_GETCHECK: Uint = 0x00f0;
    const BST_CHECKED: Lresult = 1;
    const CB_ADDSTRING: Uint = 0x0143;
    const CB_GETCURSEL: Uint = 0x0147;
    const CB_SETCURSEL: Uint = 0x014e;

    const LVM_FIRST: Uint = 0x1000;
    const LVM_DELETEALLITEMS: Uint = LVM_FIRST + 9;
    const LVM_GETNEXTITEM: Uint = LVM_FIRST + 12;
    const LVM_SETCOLUMNWIDTH: Uint = LVM_FIRST + 30;
    const LVM_SETEXTENDEDLISTVIEWSTYLE: Uint = LVM_FIRST + 54;
    const LVM_SETITEMW: Uint = LVM_FIRST + 76;
    const LVM_INSERTITEMW: Uint = LVM_FIRST + 77;
    const LVM_INSERTCOLUMNW: Uint = LVM_FIRST + 97;
    const LVNI_SELECTED: Lparam = 0x0002;
    const LVIF_TEXT: Uint = 0x0001;
    const LVCF_FMT: Uint = 0x0001;
    const LVCF_WIDTH: Uint = 0x0002;
    const LVCF_TEXT: Uint = 0x0004;
    const LVCF_SUBITEM: Uint = 0x0008;
    const LVCFMT_LEFT: i32 = 0;
    const LVIS_SELECTED: Uint = 0x0002;

    const NM_DBLCLK: i32 = -3;
    const NM_RETURN: i32 = -4;
    const LVN_ITEMCHANGED: i32 = -101;

    const ICC_LISTVIEW_CLASSES: Dword = 0x0000_0001;

    const ID_FILE_EDIT: usize = 1001;
    const ID_CONTENT_EDIT: usize = 1002;
    const ID_PATH_CHECK: usize = 1003;
    const ID_CASE_CHECK: usize = 1004;
    const ID_MODE_COMBO: usize = 1005;
    const ID_RESULTS: usize = 1006;
    const ID_PREVIEW: usize = 1007;
    const ID_STATUS: usize = 1008;
    const ID_OPEN: usize = 1009;
    const ID_REVEAL: usize = 1010;
    const ID_RELOAD: usize = 1011;
    const ID_MORE: usize = 1014;
    const SEARCH_TIMER: usize = 1;
    const RUNTIME_TIMER: usize = 2;
    const SEARCH_DEBOUNCE_MS: Uint = 140;
    const RUNTIME_POLL_MS: Uint = 750;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: Hwnd,
        message: Uint,
        w_param: Wparam,
        l_param: Lparam,
        time: Dword,
        pt: Point,
        l_private: Dword,
    }

    #[repr(C)]
    struct WndClassExW {
        cb_size: Uint,
        style: Uint,
        wnd_proc: Option<unsafe extern "system" fn(Hwnd, Uint, Wparam, Lparam) -> Lresult>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        instance: Hinstance,
        icon: Hicon,
        cursor: Hcursor,
        background: Hbrush,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: Hicon,
    }

    #[repr(C)]
    struct CreateStructW {
        create_params: *mut c_void,
        instance: Hinstance,
        menu: Hmenu,
        parent: Hwnd,
        cy: i32,
        cx: i32,
        y: i32,
        x: i32,
        style: i32,
        name: *const u16,
        class: *const u16,
        ex_style: Dword,
    }

    #[repr(C)]
    struct InitCommonControlsEx {
        size: Dword,
        icc: Dword,
    }

    #[repr(C)]
    struct NmHdr {
        hwnd_from: Hwnd,
        id_from: usize,
        code: Uint,
    }

    #[repr(C)]
    struct NmListView {
        hdr: NmHdr,
        item: i32,
        sub_item: i32,
        new_state: Uint,
        old_state: Uint,
        changed: Uint,
        action: Point,
        l_param: Lparam,
    }

    #[repr(C)]
    struct LvItemW {
        mask: Uint,
        item: i32,
        sub_item: i32,
        state: Uint,
        state_mask: Uint,
        text: *mut u16,
        text_max: i32,
        image: i32,
        l_param: Lparam,
        indent: i32,
        group_id: i32,
        columns: Uint,
        columns_ptr: *mut Uint,
        column_formats: *mut i32,
        group: i32,
    }

    #[repr(C)]
    struct LvColumnW {
        mask: Uint,
        fmt: i32,
        cx: i32,
        text: *mut u16,
        text_max: i32,
        sub_item: i32,
        image: i32,
        order: i32,
        min_width: i32,
        default_width: i32,
        ideal_width: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassExW(class: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: Dword,
            class_name: *const u16,
            window_name: *const u16,
            style: Dword,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Hwnd,
            menu: Hmenu,
            instance: Hinstance,
            param: *mut c_void,
        ) -> Hwnd;
        fn DefWindowProcW(hwnd: Hwnd, msg: Uint, w_param: Wparam, l_param: Lparam) -> Lresult;
        fn ShowWindow(hwnd: Hwnd, command: i32) -> Bool;
        fn UpdateWindow(hwnd: Hwnd) -> Bool;
        fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: Uint, max: Uint) -> Bool;
        fn TranslateMessage(msg: *const Msg) -> Bool;
        fn DispatchMessageW(msg: *const Msg) -> Lresult;
        fn PostQuitMessage(code: i32);
        fn LoadCursorW(instance: Hinstance, name: *const u16) -> Hcursor;
        fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: Lparam) -> Lparam;
        fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> Lparam;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
        fn MoveWindow(hwnd: Hwnd, x: i32, y: i32, width: i32, height: i32, repaint: Bool) -> Bool;
        fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> Bool;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
        fn SendMessageW(hwnd: Hwnd, msg: Uint, w_param: Wparam, l_param: Lparam) -> Lresult;
        fn SetTimer(hwnd: Hwnd, id: usize, elapsed_ms: Uint, callback: *mut c_void) -> usize;
        fn KillTimer(hwnd: Hwnd, id: usize) -> Bool;
        fn SetFocus(hwnd: Hwnd) -> Hwnd;
        fn PostMessageW(hwnd: Hwnd, msg: Uint, w_param: Wparam, l_param: Lparam) -> Bool;
        fn MessageBoxW(hwnd: Hwnd, text: *const u16, caption: *const u16, kind: Uint) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Hinstance;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetStockObject(object: i32) -> *mut c_void;
    }

    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn InitCommonControlsEx(config: *const InitCommonControlsEx) -> Bool;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: Hwnd,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }

    enum SearchBackend {
        Legacy(Box<GuiSearchSession>),
        App(Box<AppGuiSearchSession>),
    }

    impl SearchBackend {
        fn search(&mut self, request: &GuiSearchRequest) -> Result<GuiSearchResponse, String> {
            match self {
                Self::Legacy(session) => session.search(request).map_err(|error| error.to_string()),
                Self::App(session) => session.search(request).map_err(|error| error.to_string()),
            }
        }

        fn reload(&mut self) -> Result<GuiIndexStatus, String> {
            match self {
                Self::Legacy(session) => session.reload().map_err(|error| error.to_string()),
                Self::App(session) => session.reload().map_err(|error| error.to_string()),
            }
        }
    }

    struct LaunchContext {
        backend: Option<SearchBackend>,
        status: Option<GuiIndexStatus>,
        runtime: Option<AppRuntimeHandle>,
        runtime_reader: Option<RuntimeReader>,
        zero_config: bool,
    }

    enum WorkerCommand {
        Search(u64, GuiSearchRequest),
        Reload(u64),
        Quit,
    }

    enum WorkerResponse {
        Search(u64, Result<GuiSearchResponse, String>),
        Reload(u64, Result<GuiIndexStatus, String>),
    }

    struct AppState {
        file_label: Hwnd,
        content_label: Hwnd,
        file_edit: Hwnd,
        content_edit: Hwnd,
        path_check: Hwnd,
        case_check: Hwnd,
        mode_combo: Hwnd,
        results: Hwnd,
        preview: Hwnd,
        status: Hwnd,
        open_button: Hwnd,
        reveal_button: Hwnd,
        reload_button: Hwnd,
        more_button: Hwnd,
        worker: Sender<WorkerCommand>,
        next_request: u64,
        latest_request: u64,
        max_files: usize,
        rows: Vec<GuiResultRow>,
        index_status: GuiIndexStatus,
        runtime: Option<AppRuntimeHandle>,
        runtime_reader: Option<RuntimeReader>,
        last_runtime_revision: u64,
        zero_config: bool,
    }

    pub fn run(args: GuiArgs) -> Result<(), String> {
        let launch = match args.mode {
            GuiLaunchMode::Legacy { root, store } => {
                let session = GuiSearchSession::load(&root, &store, args.extractor)
                    .map_err(|error| error.to_string())?;
                let status = session.status();
                Box::new(LaunchContext {
                    backend: Some(SearchBackend::Legacy(Box::new(session))),
                    status: Some(status),
                    runtime: None,
                    runtime_reader: None,
                    zero_config: false,
                })
            }
            GuiLaunchMode::ZeroConfig => {
                let runtime = AppRuntimeHandle::start_default(args.extractor.clone())
                    .map_err(|error| format!("failed to start background index coordinator: {error}"))?;
                let runtime_reader = runtime.reader();
                let paths = runtime.paths().clone();
                let volumes = runtime.volumes().to_vec();
                let session = AppGuiSearchSession::load(
                    paths,
                    volumes,
                    args.extractor,
                    runtime_reader.clone(),
                )
                .map_err(|error| error.to_string())?;
                let status = session.status();
                Box::new(LaunchContext {
                    backend: Some(SearchBackend::App(Box::new(session))),
                    status: Some(status),
                    runtime: Some(runtime),
                    runtime_reader: Some(runtime_reader),
                    zero_config: true,
                })
            }
        };

        unsafe {
            let common = InitCommonControlsEx {
                size: mem::size_of::<InitCommonControlsEx>() as Dword,
                icc: ICC_LISTVIEW_CLASSES,
            };
            if InitCommonControlsEx(&common) == 0 {
                return Err("InitCommonControlsEx failed".to_string());
            }
            let instance = GetModuleHandleW(null());
            if instance.is_null() {
                return Err(last_error("GetModuleHandleW"));
            }
            let class_name = wide("PersonalRagV2GuiWindow");
            let title = wide("PersonalRag");
            let class = WndClassExW {
                cb_size: mem::size_of::<WndClassExW>() as Uint,
                style: 0,
                wnd_proc: Some(window_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                instance,
                icon: null_mut(),
                cursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
                background: (COLOR_WINDOW + 1) as Hbrush,
                menu_name: null(),
                class_name: class_name.as_ptr(),
                icon_sm: null_mut(),
            };
            if RegisterClassExW(&class) == 0 {
                return Err(last_error("RegisterClassExW"));
            }
            let launch_ptr = Box::into_raw(launch);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1180,
                760,
                null_mut(),
                null_mut(),
                instance,
                launch_ptr.cast(),
            );
            if hwnd.is_null() {
                drop(Box::from_raw(launch_ptr));
                return Err(last_error("CreateWindowExW"));
            }
            drop(Box::from_raw(launch_ptr));
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            let mut msg: Msg = mem::zeroed();
            loop {
                let result = GetMessageW(&mut msg, null_mut(), 0, 0);
                if result == -1 {
                    return Err(last_error("GetMessageW"));
                }
                if result == 0 {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    pub fn show_error(caption: &str, error: &str) {
        unsafe {
            let caption = wide(caption);
            let error = wide(error);
            MessageBoxW(null_mut(), error.as_ptr(), caption.as_ptr(), 0x10);
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: Hwnd,
        msg: Uint,
        w_param: Wparam,
        l_param: Lparam,
    ) -> Lresult {
        match msg {
            WM_CREATE => {
                let create = unsafe { &*(l_param as *const CreateStructW) };
                let launch = unsafe { &mut *create.create_params.cast::<LaunchContext>() };
                let Some(backend) = launch.backend.take() else {
                    show_error("PersonalRag V2", "GUI launch backend was already consumed");
                    return -1;
                };
                let Some(status) = launch.status.take() else {
                    show_error("PersonalRag V2", "GUI launch status was already consumed");
                    return -1;
                };
                let runtime = launch.runtime.take();
                let runtime_reader = launch.runtime_reader.take();
                let zero_config = launch.zero_config;
                match unsafe {
                    initialize_window(
                        hwnd,
                        backend,
                        status,
                        runtime,
                        runtime_reader,
                        zero_config,
                    )
                } {
                    Ok(state) => {
                        let state = Box::into_raw(Box::new(state));
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as Lparam);
                            layout(hwnd, &*state);
                            SetFocus((*state).file_edit);
                        }
                        0
                    }
                    Err(error) => {
                        show_error("PersonalRag V2", &error);
                        -1
                    }
                }
            }
            WM_SIZE => {
                if let Some(state) = unsafe { state(hwnd) } {
                    unsafe { layout(hwnd, state) };
                }
                0
            }
            WM_SETFOCUS => {
                if let Some(state) = unsafe { state(hwnd) } {
                    unsafe { SetFocus(state.file_edit) };
                }
                0
            }
            WM_COMMAND => {
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    let id = w_param & 0xffff;
                    let notification = ((w_param >> 16) & 0xffff) as u16;
                    match (id, notification) {
                        (ID_FILE_EDIT | ID_CONTENT_EDIT, EN_CHANGE) => {
                            state.max_files = personalrag_v2::gui::GUI_FIRST_BATCH_FILES;
                            unsafe { schedule_search(hwnd) };
                        }
                        (ID_PATH_CHECK | ID_CASE_CHECK, BN_CLICKED)
                        | (ID_MODE_COMBO, CBN_SELCHANGE) => {
                            state.max_files = personalrag_v2::gui::GUI_FIRST_BATCH_FILES;
                            unsafe { submit_search(hwnd, state) };
                        }
                        (ID_OPEN, BN_CLICKED) => unsafe { open_selected(state) },
                        (ID_REVEAL, BN_CLICKED) => unsafe { reveal_selected(state) },
                        (ID_RELOAD, BN_CLICKED) => unsafe { submit_reload(state) },
                        (ID_MORE, BN_CLICKED) => {
                            let visible_upper_bound = state
                                .index_status
                                .metadata_records
                                .saturating_add(state.index_status.delta_changes)
                                .max(personalrag_v2::gui::GUI_FIRST_BATCH_FILES);
                            state.max_files =
                                state.max_files.saturating_mul(2).min(visible_upper_bound);
                            unsafe { submit_search(hwnd, state) };
                        }
                        _ => {}
                    }
                }
                0
            }
            WM_TIMER if w_param == SEARCH_TIMER => {
                unsafe { KillTimer(hwnd, SEARCH_TIMER) };
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    unsafe { submit_search(hwnd, state) };
                }
                0
            }
            WM_TIMER if w_param == RUNTIME_TIMER => {
                if let Some(state) = unsafe { state_mut(hwnd) }
                    && let Some(reader) = state.runtime_reader.as_ref()
                {
                    let revision = reader.revision();
                    if revision > state.last_runtime_revision {
                        state.last_runtime_revision = revision;
                        unsafe { submit_reload(state) };
                    } else {
                        unsafe { update_idle_status(state) };
                    }
                }
                0
            }
            WM_NOTIFY => {
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    let header = unsafe { &*(l_param as *const NmHdr) };
                    if header.id_from == ID_RESULTS {
                        match header.code as i32 {
                            NM_DBLCLK | NM_RETURN => unsafe { open_selected(state) },
                            LVN_ITEMCHANGED => {
                                let event = unsafe { &*(l_param as *const NmListView) };
                                if event.item >= 0
                                    && event.new_state & LVIS_SELECTED != 0
                                    && event.old_state & LVIS_SELECTED == 0
                                {
                                    unsafe { update_preview(state, event.item as usize) };
                                }
                            }
                            _ => {}
                        }
                    }
                }
                0
            }
            WM_APP_SEARCH_DONE => {
                let response = unsafe { Box::from_raw(l_param as *mut WorkerResponse) };
                if let Some(state) = unsafe { state_mut(hwnd) }
                    && let WorkerResponse::Search(request_id, result) = *response
                    && request_id >= state.latest_request
                {
                    state.latest_request = request_id;
                    unsafe { apply_search_response(state, result) };
                }
                0
            }
            WM_APP_RELOAD_DONE => {
                let response = unsafe { Box::from_raw(l_param as *mut WorkerResponse) };
                if let Some(state) = unsafe { state_mut(hwnd) }
                    && let WorkerResponse::Reload(request_id, result) = *response
                    && request_id >= state.latest_request
                {
                    state.latest_request = request_id;
                    match result {
                        Ok(status) => {
                            state.index_status = status;
                            if let Some(reader) = state.runtime_reader.as_ref() {
                                state.last_runtime_revision = reader.revision();
                            }
                            unsafe {
                                update_idle_status(state);
                                submit_search(hwnd, state);
                            };
                        }
                        Err(error) => unsafe {
                            set_text(state.status, &format!("Reload failed: {error}"))
                        },
                    }
                }
                0
            }
            WM_DESTROY => {
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    let _ = state.worker.send(WorkerCommand::Quit);
                    if let Some(runtime) = state.runtime.as_ref() {
                        runtime.request_stop();
                    }
                    unsafe {
                        KillTimer(hwnd, RUNTIME_TIMER);
                    }
                }
                unsafe { PostQuitMessage(0) };
                0
            }
            WM_NCDESTROY => {
                let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut AppState;
                if !raw.is_null() {
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                        drop(Box::from_raw(raw));
                    }
                }
                unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) },
        }
    }

    unsafe fn initialize_window(
        hwnd: Hwnd,
        backend: SearchBackend,
        index_status: GuiIndexStatus,
        runtime: Option<AppRuntimeHandle>,
        runtime_reader: Option<RuntimeReader>,
        zero_config: bool,
    ) -> Result<AppState, String> {
        let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) as Hfont };
        let instance = unsafe { GetModuleHandleW(null()) };
        let static_class = wide("STATIC");
        let edit_class = wide("EDIT");
        let button_class = wide("BUTTON");
        let combo_class = wide("COMBOBOX");
        let list_class = wide("SysListView32");

        let file_label = unsafe {
            child(
                hwnd,
                instance,
                &static_class,
                "File / path",
                WS_CHILD | WS_VISIBLE,
                0,
            )
        };
        let file_edit = unsafe {
            child_with_id(
                hwnd,
                instance,
                &edit_class,
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
                ID_FILE_EDIT,
            )
        }?;
        let path_check = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "Full path",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX,
                ID_PATH_CHECK,
            )
        }?;
        let case_check = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "Case sensitive",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX,
                ID_CASE_CHECK,
            )
        }?;
        let content_label = unsafe {
            child(
                hwnd,
                instance,
                &static_class,
                "Content",
                WS_CHILD | WS_VISIBLE,
                0,
            )
        };
        let content_edit = unsafe {
            child_with_id(
                hwnd,
                instance,
                &edit_class,
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
                ID_CONTENT_EDIT,
            )
        }?;
        let mode_combo = unsafe {
            child_with_id(
                hwnd,
                instance,
                &combo_class,
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST,
                ID_MODE_COMBO,
            )
        }?;
        for value in ["Literal", "Regex", "Wildcard"] {
            let value = wide(value);
            unsafe { SendMessageW(mode_combo, CB_ADDSTRING, 0, value.as_ptr() as Lparam) };
        }
        unsafe { SendMessageW(mode_combo, CB_SETCURSEL, 0, 0) };

        let open_button = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "Open",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                ID_OPEN,
            )
        }?;
        let reveal_button = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "Show in Explorer",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                ID_REVEAL,
            )
        }?;
        let reload_button = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "Reload index",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                ID_RELOAD,
            )
        }?;
        let more_button = unsafe {
            child_with_id(
                hwnd,
                instance,
                &button_class,
                "More",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                ID_MORE,
            )
        }?;
        let results = unsafe {
            child_with_id(
                hwnd,
                instance,
                &list_class,
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | LVS_REPORT
                    | LVS_SINGLESEL
                    | LVS_SHOWSELALWAYS,
                ID_RESULTS,
            )
        }?;
        unsafe {
            SendMessageW(
                results,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                0,
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_GRIDLINES) as Lparam,
            );
        }
        unsafe { initialize_columns(results) };

        let preview = unsafe {
            child_with_id(
                hwnd,
                instance,
                &edit_class,
                "Select a result to preview the first logical hit.",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | WS_VSCROLL
                    | ES_MULTILINE
                    | ES_AUTOVSCROLL
                    | ES_READONLY,
                ID_PREVIEW,
            )
        }?;
        let status = unsafe {
            child_with_id(
                hwnd,
                instance,
                &static_class,
                "",
                WS_CHILD | WS_VISIBLE,
                ID_STATUS,
            )
        }?;

        for control in [
            file_label,
            content_label,
            file_edit,
            path_check,
            case_check,
            content_edit,
            mode_combo,
            open_button,
            reveal_button,
            reload_button,
            more_button,
            results,
            preview,
            status,
        ] {
            unsafe { SendMessageW(control, WM_SETFONT, font as Wparam, TRUE as Lparam) };
        }

        let (sender, receiver) = mpsc::channel::<WorkerCommand>();
        let worker_hwnd = hwnd as usize;
        thread::spawn(move || worker_loop(worker_hwnd as Hwnd, backend, receiver));

        let state = AppState {
            file_label,
            content_label,
            file_edit,
            content_edit,
            path_check,
            case_check,
            mode_combo,
            results,
            preview,
            status,
            open_button,
            reveal_button,
            reload_button,
            more_button,
            worker: sender,
            next_request: 0,
            latest_request: 0,
            max_files: personalrag_v2::gui::GUI_FIRST_BATCH_FILES,
            rows: Vec::new(),
            index_status,
            last_runtime_revision: runtime_reader.as_ref().map_or(0, RuntimeReader::revision),
            runtime,
            runtime_reader,
            zero_config,
        };
        if state.zero_config {
            unsafe {
                SetTimer(hwnd, RUNTIME_TIMER, RUNTIME_POLL_MS, null_mut());
            }
        }
        unsafe { update_idle_status(&state) };
        Ok(state)
    }

    fn worker_loop(hwnd: Hwnd, mut backend: SearchBackend, receiver: Receiver<WorkerCommand>) {
        while let Ok(command) = receiver.recv() {
            match command {
                WorkerCommand::Search(request_id, request) => {
                    let result = backend.search(&request);
                    post_worker_response(
                        hwnd,
                        WM_APP_SEARCH_DONE,
                        WorkerResponse::Search(request_id, result),
                    );
                }
                WorkerCommand::Reload(request_id) => {
                    let result = backend.reload();
                    post_worker_response(
                        hwnd,
                        WM_APP_RELOAD_DONE,
                        WorkerResponse::Reload(request_id, result),
                    );
                }
                WorkerCommand::Quit => break,
            }
        }
    }

    fn post_worker_response(hwnd: Hwnd, message: Uint, response: WorkerResponse) {
        let raw = Box::into_raw(Box::new(response));
        let ok = unsafe { PostMessageW(hwnd, message, 0, raw as Lparam) };
        if ok == 0 {
            unsafe { drop(Box::from_raw(raw)) };
        }
    }

    unsafe fn schedule_search(hwnd: Hwnd) {
        unsafe {
            KillTimer(hwnd, SEARCH_TIMER);
            SetTimer(hwnd, SEARCH_TIMER, SEARCH_DEBOUNCE_MS, null_mut());
        }
    }

    unsafe fn submit_search(hwnd: Hwnd, state: &mut AppState) {
        unsafe { KillTimer(hwnd, SEARCH_TIMER) };
        state.next_request = state.next_request.saturating_add(1);
        let request_id = state.next_request;
        state.latest_request = request_id;
        let request = GuiSearchRequest {
            file_query: unsafe { get_text(state.file_edit) },
            content_query: unsafe { get_text(state.content_edit) },
            file_scope: if unsafe { SendMessageW(state.path_check, BM_GETCHECK, 0, 0) }
                == BST_CHECKED
            {
                GuiFileScope::FullPath
            } else {
                GuiFileScope::Filename
            },
            content_mode: match unsafe { SendMessageW(state.mode_combo, CB_GETCURSEL, 0, 0) } {
                1 => GuiContentMode::Regex,
                2 => GuiContentMode::Wildcard,
                _ => GuiContentMode::Literal,
            },
            case_sensitive: unsafe { SendMessageW(state.case_check, BM_GETCHECK, 0, 0) }
                == BST_CHECKED,
            max_files: state.max_files,
        };
        unsafe { set_text(state.status, "Searching…") };
        if state
            .worker
            .send(WorkerCommand::Search(request_id, request))
            .is_err()
        {
            unsafe { set_text(state.status, "Search worker is not available") };
        }
    }

    unsafe fn submit_reload(state: &mut AppState) {
        state.next_request = state.next_request.saturating_add(1);
        let request_id = state.next_request;
        state.latest_request = request_id;
        unsafe { set_text(state.status, "Reloading index bundle…") };
        if state
            .worker
            .send(WorkerCommand::Reload(request_id))
            .is_err()
        {
            unsafe { set_text(state.status, "Search worker is not available") };
        }
    }

    unsafe fn apply_search_response(
        state: &mut AppState,
        result: Result<GuiSearchResponse, String>,
    ) {
        match result {
            Ok(response) => {
                state.rows = response.rows;
                unsafe { fill_results(state) };
                let elapsed = response.stats.elapsed.as_secs_f64() * 1000.0;
                unsafe {
                    set_text(
                        state.status,
                        &format!(
                            "{} files · {:.1} ms · limit {} · bundle {} · metadata {} · delta {}",
                            response.stats.returned_files,
                            elapsed,
                            state.max_files,
                            response.stats.bundle_generation,
                            state.index_status.metadata_records,
                            state.index_status.delta_changes
                        ),
                    )
                };
            }
            Err(error) => {
                state.rows.clear();
                unsafe {
                    SendMessageW(state.results, LVM_DELETEALLITEMS, 0, 0);
                    set_text(state.preview, "");
                    set_text(state.status, &format!("Search failed: {error}"));
                }
            }
        }
    }

    unsafe fn fill_results(state: &AppState) {
        unsafe { SendMessageW(state.results, LVM_DELETEALLITEMS, 0, 0) };
        for (index, row) in state.rows.iter().enumerate() {
            let matches = if row.matches.is_empty() {
                String::new()
            } else {
                row.visible_match_count().to_string()
            };
            let values = [
                row.name.clone(),
                row.relative_path.to_string_lossy().into_owned(),
                matches,
                row.primary_location().to_string(),
                format_file_size(row.size),
                format_modified_ns_utc(row.modified_ns),
            ];
            unsafe { insert_list_row(state.results, index as i32, &values) };
        }
        if let Some(first) = state.rows.first() {
            unsafe { set_text(state.preview, first.primary_preview()) };
        } else {
            unsafe { set_text(state.preview, "") };
        }
    }

    unsafe fn update_preview(state: &AppState, index: usize) {
        if let Some(row) = state.rows.get(index) {
            unsafe { set_text(state.preview, row.primary_preview()) };
        }
    }

    unsafe fn open_selected(state: &AppState) {
        let Some(row) = (unsafe { selected_row(state) }) else {
            return;
        };
        let operation = wide("open");
        let path = path_wide(&row.absolute_path);
        unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                SW_SHOW,
            );
        }
    }

    unsafe fn reveal_selected(state: &AppState) {
        let Some(row) = (unsafe { selected_row(state) }) else {
            return;
        };
        let explorer = wide("explorer.exe");
        let path = row.absolute_path.to_string_lossy();
        let params = wide(&format!("/select,\"{path}\""));
        unsafe {
            ShellExecuteW(
                null_mut(),
                null(),
                explorer.as_ptr(),
                params.as_ptr(),
                null(),
                SW_SHOW,
            );
        }
    }

    unsafe fn selected_row(state: &AppState) -> Option<&GuiResultRow> {
        let index =
            unsafe { SendMessageW(state.results, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED) };
        if index < 0 {
            state.rows.first()
        } else {
            state.rows.get(index as usize)
        }
    }

    unsafe fn initialize_columns(list: Hwnd) {
        for (index, (name, width)) in [
            ("Name", 190),
            ("Path", 470),
            ("Hits", 65),
            ("Location", 135),
            ("Size", 95),
            ("Modified (UTC)", 155),
        ]
        .into_iter()
        .enumerate()
        {
            let mut text = wide(name);
            let mut column = LvColumnW {
                mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT | LVCF_SUBITEM,
                fmt: LVCFMT_LEFT,
                cx: width,
                text: text.as_mut_ptr(),
                text_max: text.len() as i32,
                sub_item: index as i32,
                image: 0,
                order: index as i32,
                min_width: 0,
                default_width: 0,
                ideal_width: 0,
            };
            unsafe {
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    index,
                    (&mut column as *mut LvColumnW) as Lparam,
                );
            }
        }
    }

    unsafe fn insert_list_row(list: Hwnd, row: i32, values: &[String; 6]) {
        for (sub_item, value) in values.iter().enumerate() {
            let mut text = wide(value);
            let mut item = LvItemW {
                mask: LVIF_TEXT,
                item: row,
                sub_item: sub_item as i32,
                state: 0,
                state_mask: 0,
                text: text.as_mut_ptr(),
                text_max: text.len() as i32,
                image: 0,
                l_param: 0,
                indent: 0,
                group_id: 0,
                columns: 0,
                columns_ptr: null_mut(),
                column_formats: null_mut(),
                group: 0,
            };
            unsafe {
                SendMessageW(
                    list,
                    if sub_item == 0 {
                        LVM_INSERTITEMW
                    } else {
                        LVM_SETITEMW
                    },
                    0,
                    (&mut item as *mut LvItemW) as Lparam,
                );
            }
        }
    }

    unsafe fn layout(hwnd: Hwnd, state: &AppState) {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
            return;
        }
        let width = (rect.right - rect.left).max(760);
        let height = (rect.bottom - rect.top).max(520);
        let margin = 10;
        let label = 72;
        let row_h = 27;
        let controls_right = 245;
        let edit_w = (width - margin * 2 - label - controls_right).max(240);
        let edit_x = margin + label;
        unsafe {
            move_control(state.file_label, margin, margin + 5, label - 4, row_h);
            move_control(state.content_label, margin, margin + 39, label - 4, row_h);
            move_control(state.file_edit, edit_x, margin, edit_w, row_h);
            move_control(state.path_check, edit_x + edit_w + 8, margin + 2, 88, row_h);
            move_control(
                state.case_check,
                edit_x + edit_w + 98,
                margin + 2,
                125,
                row_h,
            );
            move_control(state.content_edit, edit_x, margin + 34, edit_w, row_h);
            move_control(state.mode_combo, edit_x + edit_w + 8, margin + 34, 110, 180);
            move_control(state.open_button, margin, margin + 70, 72, 28);
            move_control(state.reveal_button, margin + 80, margin + 70, 128, 28);
            move_control(state.reload_button, margin + 216, margin + 70, 105, 28);
            move_control(state.more_button, margin + 329, margin + 70, 72, 28);
        }

        let status_h = 22;
        let preview_h = (height / 5).clamp(105, 180);
        let list_top = margin + 106;
        let status_y = height - status_h - margin;
        let preview_y = status_y - preview_h - 6;
        let list_h = (preview_y - list_top - 6).max(120);
        unsafe {
            move_control(state.results, margin, list_top, width - margin * 2, list_h);
            move_control(
                state.preview,
                margin,
                preview_y,
                width - margin * 2,
                preview_h,
            );
            move_control(state.status, margin, status_y, width - margin * 2, status_h);
            SendMessageW(
                state.results,
                LVM_SETCOLUMNWIDTH,
                1,
                (width - 700).max(260) as Lparam,
            );
        }
    }

    unsafe fn update_idle_status(state: &AppState) {
        let text = if let Some(reader) = state.runtime_reader.as_ref() {
            runtime_status_text(&reader.snapshot())
        } else {
            format!(
                "Ready · bundle {} · metadata {} · delta {} · root {}",
                state.index_status.bundle_generation,
                state.index_status.metadata_records,
                state.index_status.delta_changes,
                state.index_status.root.display()
            )
        };
        unsafe { set_text(state.status, &text) };
    }

    fn runtime_status_text(snapshot: &RuntimeSnapshot) -> String {
        let metadata_records = snapshot
            .volumes
            .iter()
            .map(|value| value.metadata_records)
            .sum::<usize>();
        let content_indexed = snapshot
            .volumes
            .iter()
            .map(|value| value.content_indexed_files)
            .sum::<usize>();
        let content_total = snapshot
            .volumes
            .iter()
            .map(|value| value.content_total_files)
            .sum::<usize>();
        let errors = snapshot
            .volumes
            .iter()
            .filter(|value| value.last_error.is_some())
            .count();
        if snapshot.metadata_ready_volumes < snapshot.total_volumes {
            format!(
                "Files: {}/{} drives ready · {} items · Content: waiting · background indexing{}",
                snapshot.metadata_ready_volumes,
                snapshot.total_volumes,
                metadata_records,
                if errors == 0 {
                    String::new()
                } else {
                    format!(" · {errors} degraded")
                }
            )
        } else if snapshot.content_ready_volumes < snapshot.total_volumes {
            format!(
                "Files: Ready · {} items · Content: {}/{} files · {}/{} drives ready{}",
                metadata_records,
                content_indexed,
                content_total,
                snapshot.content_ready_volumes,
                snapshot.total_volumes,
                if errors == 0 {
                    String::new()
                } else {
                    format!(" · {errors} degraded")
                }
            )
        } else {
            format!(
                "Ready · {} files · Content {}/{} · watching {} drives{}",
                metadata_records,
                content_indexed,
                content_total,
                snapshot.total_volumes,
                if errors == 0 {
                    String::new()
                } else {
                    format!(" · {errors} degraded")
                }
            )
        }
    }

    unsafe fn child(
        parent: Hwnd,
        instance: Hinstance,
        class: &[u16],
        text: &str,
        style: Dword,
        id: usize,
    ) -> Hwnd {
        let text = wide(text);
        unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                text.as_ptr(),
                style,
                0,
                0,
                10,
                10,
                parent,
                id as Hmenu,
                instance,
                null_mut(),
            )
        }
    }

    unsafe fn child_with_id(
        parent: Hwnd,
        instance: Hinstance,
        class: &[u16],
        text: &str,
        style: Dword,
        id: usize,
    ) -> Result<Hwnd, String> {
        let hwnd = unsafe { child(parent, instance, class, text, style, id) };
        if hwnd.is_null() {
            Err(last_error("CreateWindowExW child"))
        } else {
            Ok(hwnd)
        }
    }

    unsafe fn move_control(hwnd: Hwnd, x: i32, y: i32, width: i32, height: i32) {
        unsafe { MoveWindow(hwnd, x, y, width.max(1), height.max(1), TRUE) };
    }

    unsafe fn state(hwnd: Hwnd) -> Option<&'static AppState> {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const AppState;
        unsafe { raw.as_ref() }
    }

    unsafe fn state_mut(hwnd: Hwnd) -> Option<&'static mut AppState> {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut AppState;
        unsafe { raw.as_mut() }
    }

    unsafe fn set_text(hwnd: Hwnd, value: &str) {
        let value = wide(value);
        unsafe { SetWindowTextW(hwnd, value.as_ptr()) };
    }

    unsafe fn get_text(hwnd: Hwnd) -> String {
        let len = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
        let mut buffer = vec![0_u16; len + 1];
        unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..len])
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[cfg(target_os = "windows")]
    fn path_wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    #[cfg(all(windows, not(target_os = "windows")))]
    fn path_wide(path: &Path) -> Vec<u16> {
        path.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_error(context: &str) -> String {
        format!("{context} failed: {}", std::io::Error::last_os_error())
    }
}
