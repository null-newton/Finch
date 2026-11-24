//! Finch desktop: starts finch's own web server on a loopback port inside this process and shows it in a native window. The page gets
//! no access to the Tauri API; the signed update check runs here (opt-in `[update] check = true`, or the menu's "Check for updates…"),
//! goes through finch's egress policy and journal, and installs only after the user says yes.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use finch::{config::Config, db, egress, net, web};
use std::{net::TcpListener, path::PathBuf, sync::Arc, time::Duration};
use tauri::{menu::{Menu, MenuItem, PredefinedMenuItem, Submenu}, AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

struct State { cfg: Config, port: u16, app: Arc<web::App> }

fn say(app: &AppHandle, kind: MessageDialogKind, text: &str) { app.dialog().message(text).title("Finch").kind(kind).blocking_show(); }
fn open_in_browser(url: &str) { let _ = if cfg!(windows) { std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn() } else if cfg!(target_os = "macos") { std::process::Command::new("open").arg(url).spawn() } else { std::process::Command::new("xdg-open").arg(url).spawn() }; }
/// The configured port, or a free one when it is taken (a `finch ui` may already run).
fn free_port(want: u16) -> u16 { if TcpListener::bind(("127.0.0.1", want)).is_ok() { want } else { TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(want) } }

/// Checks GitHub for a newer signed build. `manual`: started from the menu (it runs even when the automatic check is off, and always answers).
fn check_updates(app: AppHandle, manual: bool) {
    std::thread::spawn(move || {
        let st = app.state::<State>(); if !manual && !st.cfg.update_check { return; }
        if let Some(w) = egress::evaluate(&st.cfg, "update.github", "github.com") { if manual { say(&app, MessageDialogKind::Warning, &format!("The update check is refused: {w}")); } return; }
        let con = db::connect(&st.cfg, db::Opts { migrate: false, ..Default::default() }).ok(); if egress::allow(con.as_ref(), &st.cfg, "update.github", "github.com", 0, "update.check").is_err() { return; }
        let res = tauri::async_runtime::block_on(async { app.updater()?.check().await });
        match res {
            Ok(Some(u)) => {
                let yes = app.dialog().message(format!("Finch {} is available (you have {}).\n\nInstall it now? Finch restarts afterwards; your data stays where it is.", u.version, u.current_version)).title("Finch update")
                    .buttons(MessageDialogButtons::OkCancelCustom("Install and restart".into(), "Later".into())).blocking_show();
                if !yes { return; }
                match tauri::async_runtime::block_on(u.download_and_install(|_, _| {}, || {})) { Ok(()) => app.restart(), Err(e) => say(&app, MessageDialogKind::Error, &format!("The update could not be installed: {e}")) }
            }
            Ok(None) => if manual { say(&app, MessageDialogKind::Info, &format!("Finch {} is up to date.", app.package_info().version)) },
            Err(e) => if manual { say(&app, MessageDialogKind::Error, &format!("The update check failed: {e}")) },
        }
    });
}

fn start(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let h = app.handle().clone();
    // its own home in the OS app-data folder (COACH_HOME / COACH_CONFIG point it elsewhere), with its own keychain entries;
    // the command line started by the coach inherits both
    let root = match (std::env::var_os("COACH_CONFIG"), std::env::var_os("COACH_HOME")) {
        (Some(c), _) => PathBuf::from(c).parent().map(PathBuf::from).unwrap_or_default(), (None, Some(r)) => PathBuf::from(r),
        _ => { let r = h.path().app_data_dir()?; std::env::set_var("COACH_HOME", &r); std::env::set_var("COACH_KEYCHAIN_SERVICE", "finch-desktop"); r } };
    let cfg = match finch::home::ensure(&root) { Ok(c) => c, Err(e) => { say(&h, MessageDialogKind::Error, &format!("Finch cannot set up or open its database in {}:\n\n{e:#}", root.display())); std::process::exit(1); } };
    // the coach starts `finch mcp serve`: the command-line program installed next to this app
    if let Some(cli) = std::env::current_exe()?.parent().map(|d| d.join(if cfg!(windows) { "finch.exe" } else { "finch" })).filter(|p| p.is_file()) { std::env::set_var("FINCH_CLI", cli); }
    let res = h.path().resource_dir().map(|d| d.join("static")).ok().filter(|d| d.join("index.html").is_file());
    let sdir = res.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../static"));
    let port = free_port(cfg.ui_port as u16);
    let wapp = Arc::new(web::App::new(cfg.clone(), false, port, false, sdir, Arc::new(|| Box::new(net::UreqHttp::new(60)) as Box<dyn net::Http>))?); let _ = wapp.me.set(Arc::downgrade(&wapp));
    let srv = wapp.clone(); let host = cfg.ui_host.clone(); std::thread::spawn(move || { let _ = web::serve(srv, &host, port); });
    for _ in 0..100 { if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; } std::thread::sleep(Duration::from_millis(50)); }
    let url = web::login_url(port, false, &wapp.sec.tokens.issue(None)?);
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?)).title("Finch").inner_size(1320.0, 900.0).min_inner_size(900.0, 620.0)
        // only finch's own local pages load in the window; any other link opens in the system browser
        .on_navigation(move |u| { let ok = matches!(u.host_str(), Some("127.0.0.1" | "localhost")) && u.port() == Some(port); if !ok && ["http", "https", "mailto"].contains(&u.scheme()) { open_in_browser(u.as_str()); } ok })
        .build()?;
    app.manage(State { cfg, port, app: wapp });
    let menu = Menu::with_items(app, &[&Submenu::with_items(app, "Finch", true, &[&MenuItem::with_id(app, "updates", "Check for updates…", true, None::<&str>)?, &MenuItem::with_id(app, "browser", "Open in browser", true, None::<&str>)?,
        &PredefinedMenuItem::separator(app)?, &PredefinedMenuItem::quit(app, Some("Quit Finch"))?])?])?; app.set_menu(menu)?;
    let h2 = h.clone(); std::thread::spawn(move || { std::thread::sleep(Duration::from_secs(5)); loop { check_updates(h2.clone(), false); std::thread::sleep(Duration::from_secs(6 * 3600)); } });
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| { if let Some(w) = app.get_webview_window("main") { let _ = w.unminimize(); let _ = w.set_focus(); } }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| start(app))
        .on_menu_event(|app, e| match e.id().as_ref() {
            "updates" => check_updates(app.clone(), true),
            "browser" => { let st = app.state::<State>(); if let Ok(t) = st.app.sec.tokens.issue(None) { open_in_browser(&web::login_url(st.port, false, &t)); } }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running Finch");
}
