use crate::client::Client;
use crate::engine::Engine;
use crate::protocol::LABEL;
use crate::server::{Config, Server};
use crate::win;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
    ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, DELETE};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
    SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, FOLDERID_ProgramFiles};

pub const PIPE: &str = r"\\.\pipe\com.the-cabinet.helper";
pub const SERVICE: &str = LABEL;
pub const DISPLAY_NAME: &str = "The Cabinet Helper";
pub const TOOLS_FOLDER: &str = "The Cabinet Helper";
pub const PROGRAM: &str = "com.the-cabinet.helper.exe";
pub const LOG: &str = "helper.log";
pub const APP_DATA: &str = r"AppData\Roaming\com.the-cabinet.app";
const DESCRIPTION: &str = "Makes the system changes an approved cartridge needs for a session of The Cabinet, and undoes them afterwards.";
const PRIVATE: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
const STARTUP: Duration = Duration::from_secs(10);
const STOPPING: Duration = Duration::from_secs(10);

pub fn tools_dir() -> Result<PathBuf, String> {
    Ok(win::known_folder(&FOLDERID_ProgramFiles)?.join(TOOLS_FOLDER))
}

pub fn installed() -> Result<PathBuf, String> {
    Ok(tools_dir()?.join(PROGRAM))
}

pub fn state_dir() -> Result<PathBuf, String> {
    Ok(win::known_folder(&FOLDERID_ProgramData)?.join(LABEL))
}

pub fn hosts() -> Result<PathBuf, String> {
    Ok(win::windows_dir()?.join(r"System32\drivers\etc\hosts"))
}

pub fn command_line(program: &Path, user: &str) -> String {
    format!("\"{}\" daemon --sid {user}", program.display())
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager(access: u32) -> Result<Handle, String> {
    let handle = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    if handle.is_null() {
        return Err(win::last_error("cannot open the service manager"));
    }
    Ok(Handle(handle))
}

fn service(manager: &Handle, access: u32) -> Result<Option<Handle>, String> {
    let handle = unsafe { OpenServiceW(manager.0, win::wide(SERVICE).as_ptr(), access) };
    if !handle.is_null() {
        return Ok(Some(Handle(handle)));
    }
    if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
        return Ok(None);
    }
    Err(win::last_error(&format!(
        "cannot open the {SERVICE} service"
    )))
}

fn query(service: &Handle) -> Result<SERVICE_STATUS_PROCESS, String> {
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(win::last_error(&format!(
            "cannot ask how the {SERVICE} service is"
        )));
    }
    Ok(status)
}

pub fn is_installed() -> bool {
    manager(SC_MANAGER_CONNECT)
        .and_then(|manager| service(&manager, SERVICE_QUERY_STATUS))
        .is_ok_and(|service| service.is_some())
}

pub fn service_process() -> Result<u32, String> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let service = service(&manager, SERVICE_QUERY_STATUS)?
        .ok_or_else(|| "The Cabinet's helper is not installed".to_string())?;
    let status = query(&service)?;
    if status.dwCurrentState != SERVICE_RUNNING || status.dwProcessId == 0 {
        return Err("The Cabinet's helper service is not running".to_string());
    }
    Ok(status.dwProcessId)
}

pub fn connect() -> Result<Client, String> {
    let expected = service_process()?;
    Client::connect_checked(Path::new(PIPE), |server| {
        if server == expected {
            Ok(())
        } else {
            Err(format!(
                "the helper's pipe is held by process {server}, not by the {SERVICE} service ({expected}); refusing to use it"
            ))
        }
    })
}

fn secure_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        let owner = win::owner_of(dir)?;
        if owner == win::SYSTEM || owner == win::ADMINISTRATORS {
            return Ok(());
        }
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let aside = dir.with_file_name(format!("{LABEL}.untrusted-{seconds}"));
        log::warn!(
            "{} belongs to {owner}, not to the system; setting it aside as {}",
            dir.display(),
            aside.display()
        );
        std::fs::rename(dir, &aside)
            .map_err(|err| format!("cannot set {} aside: {err}", dir.display()))?;
    }
    let descriptor = win::Descriptor::parse(PRIVATE)?;
    let attributes = descriptor.attributes();
    if unsafe { CreateDirectoryW(win::wide(dir).as_ptr(), &attributes) } == 0 {
        return Err(win::last_error(&format!("cannot create {}", dir.display())));
    }
    Ok(())
}

fn bind(user: &str) -> Result<Server, String> {
    let state_dir = state_dir()?;
    secure_dir(&state_dir)?;
    crate::log_to(&state_dir.join(LOG));
    Server::bind(Config {
        endpoint: PIPE.into(),
        state_dir,
        hosts: hosts()?,
        user: Some(user.to_string()),
        flush: true,
        programs: None,
        anchors: None,
    })
}

static USER: OnceLock<String> = OnceLock::new();
static STATUS: AtomicUsize = AtomicUsize::new(0);

fn report(state: u32, failure: u32) {
    let handle = STATUS.load(Ordering::SeqCst) as *mut c_void;
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if failure == 0 {
            NO_ERROR
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR
        },
        dwServiceSpecificExitCode: failure,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
            STARTUP.as_millis() as u32
        } else {
            0
        },
    };
    unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn control(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            log::info!("stopping");
            report(SERVICE_STOP_PENDING, 0);
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                report(SERVICE_STOPPED, 0);
                std::process::exit(0);
            });
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let handle =
        RegisterServiceCtrlHandlerExW(win::wide(SERVICE).as_ptr(), Some(control), std::ptr::null());
    if handle.is_null() {
        log::error!(
            "{}",
            win::last_error("cannot register with the service manager")
        );
        return;
    }
    STATUS.store(handle as usize, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0);
    let user = USER.get().cloned().unwrap_or_default();
    match bind(&user) {
        Ok(server) => {
            report(SERVICE_RUNNING, 0);
            log::info!("listening on {PIPE} for {user}");
            server.run();
        }
        Err(err) => {
            log::error!("{err}");
            report(SERVICE_STOPPED, 1);
        }
    }
}

pub fn daemon(user: &str) -> Result<(), String> {
    win::check_sid(user)?;
    let _ = USER.set(user.to_string());
    let mut name = win::wide(SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return Ok(());
    }
    if unsafe { GetLastError() } != ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
        return Err(win::last_error("cannot start as a service"));
    }
    log::info!("not started by the service manager; serving in the foreground");
    let server = bind(user)?;
    log::info!("listening on {PIPE} for {user}");
    server.run();
    Ok(())
}

fn stop(service: &Handle) -> Result<(), String> {
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    let refused = if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        Some(unsafe { GetLastError() }).filter(|code| *code != ERROR_SERVICE_NOT_ACTIVE)
    } else {
        None
    };
    let deadline = Instant::now() + STOPPING;
    while query(service)?.dwCurrentState != SERVICE_STOPPED {
        if Instant::now() > deadline {
            return Err(match refused {
                Some(code) => win::error(&format!("cannot stop the {SERVICE} service"), code),
                None => format!("the {SERVICE} service did not stop"),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn retrying(what: impl Fn() -> std::io::Result<()>) -> std::io::Result<()> {
    let deadline = Instant::now() + STOPPING;
    loop {
        match what() {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            other => return other,
        }
    }
}

fn place(path: &Path, content: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("installing");
    let _ = std::fs::remove_file(&temp);
    std::fs::write(&temp, content)
        .and_then(|()| retrying(|| std::fs::rename(&temp, path)))
        .map_err(|err| {
            let _ = std::fs::remove_file(&temp);
            format!("cannot write {}: {err}", path.display())
        })
}

fn configure(service: &Handle) -> Result<(), String> {
    let mut description = win::wide(DESCRIPTION);
    let described = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    let mut actions = [SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 1000,
    }; 3];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    for (level, info) in [
        (
            SERVICE_CONFIG_DESCRIPTION,
            (&described as *const SERVICE_DESCRIPTIONW).cast::<c_void>(),
        ),
        (
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&failure as *const SERVICE_FAILURE_ACTIONSW).cast::<c_void>(),
        ),
    ] {
        if unsafe { ChangeServiceConfig2W(service.0, level, info) } == 0 {
            return Err(win::last_error(&format!(
                "cannot configure the {SERVICE} service"
            )));
        }
    }
    Ok(())
}

pub fn install(user: &str) -> Result<(), String> {
    win::check_sid(user)?;
    if win::profile(user).is_none() {
        return Err(format!(
            "{user} is not a user with a profile on this machine"
        ));
    }
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let source = std::env::current_exe().map_err(|err| format!("cannot find myself: {err}"))?;
    let binary =
        std::fs::read(&source).map_err(|err| format!("cannot read {}: {err}", source.display()))?;
    let existing = service(&manager, SERVICE_ALL_ACCESS)?;
    if let Some(service) = &existing {
        stop(service)?;
    }
    let dir = tools_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let program = installed()?;
    place(&program, &binary)?;
    secure_dir(&state_dir()?)?;
    let command = win::wide(command_line(&program, user));
    let display = win::wide(DISPLAY_NAME);
    let service = match existing {
        Some(service) => {
            if unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    display.as_ptr(),
                )
            } == 0
            {
                return Err(win::last_error(&format!(
                    "cannot update the {SERVICE} service"
                )));
            }
            service
        }
        None => {
            let handle = unsafe {
                CreateServiceW(
                    manager.0,
                    win::wide(SERVICE).as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if handle.is_null() {
                return Err(win::last_error(&format!(
                    "cannot create the {SERVICE} service"
                )));
            }
            Handle(handle)
        }
    };
    configure(&service)?;
    if unsafe { StartServiceW(service.0, 0, std::ptr::null()) } == 0 {
        let code = unsafe { GetLastError() };
        if code != ERROR_SERVICE_ALREADY_RUNNING {
            return Err(win::error(
                &format!("cannot start the {SERVICE} service"),
                code,
            ));
        }
    }
    let deadline = Instant::now() + STARTUP;
    loop {
        match connect().and_then(|mut client| client.hello()) {
            Ok(_) => return Ok(()),
            Err(err) if Instant::now() > deadline => {
                return Err(format!(
                    "the helper service did not start ({err}); see {}",
                    state_dir()?.join(LOG).display()
                ))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

pub fn uninstall() -> Result<(), String> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let service = service(&manager, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE)?;
    if let Some(service) = &service {
        stop(service)?;
    }
    let state_dir = state_dir()?;
    let mut problems = Vec::new();
    if state_dir.exists() {
        match Engine::open(&state_dir, &hosts()?, true) {
            Ok(mut engine) => {
                let (_, failures) = engine.undo_where(|_| true);
                problems.extend(failures);
                if let Err(err) = engine.drop_orphans() {
                    problems.push(err);
                }
            }
            Err(err) => problems.push(err),
        }
    }
    if !problems.is_empty() {
        return Err(format!(
            "some changes could not be undone, so the journal in {} is kept: {}",
            state_dir.display(),
            problems.join("; ")
        ));
    }
    if let Some(service) = &service {
        if unsafe { DeleteService(service.0) } == 0 {
            problems.push(win::last_error(&format!(
                "cannot remove the {SERVICE} service"
            )));
        }
    }
    let program = installed()?;
    if std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .is_some_and(|exe| program.canonicalize().is_ok_and(|program| program == exe))
    {
        problems.push(format!(
            "{} cannot remove itself; run the uninstall from another copy",
            program.display()
        ));
    } else if let Err(err) = retrying(|| std::fs::remove_file(&program)) {
        if err.kind() != std::io::ErrorKind::NotFound {
            problems.push(format!("cannot remove {}: {err}", program.display()));
        }
    }
    if let Ok(dir) = tools_dir() {
        let _ = std::fs::remove_dir(dir);
    }
    if let Err(err) = retrying(|| std::fs::remove_dir_all(&state_dir)) {
        if err.kind() != std::io::ErrorKind::NotFound {
            problems.push(format!("cannot remove {}: {err}", state_dir.display()));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_runs_the_installed_helper_for_one_user() {
        let program = installed().unwrap();
        assert!(program.ends_with(r"The Cabinet Helper\com.the-cabinet.helper.exe"));
        assert_eq!(
            command_line(&program, "S-1-5-21-1-2-3-1001"),
            format!("\"{}\" daemon --sid S-1-5-21-1-2-3-1001", program.display())
        );
        assert!(state_dir().unwrap().ends_with("com.the-cabinet.helper"));
        assert!(hosts().unwrap().is_file());
    }

    #[test]
    #[ignore = "needs The Cabinet's helper installed as a service on this machine"]
    fn the_installed_helper_answers_only_on_its_service_pipe() {
        let mut client = connect().unwrap();
        assert_eq!(client.hello().unwrap(), crate::protocol::VERSION);
        assert!(client.status().unwrap().is_empty());
        let me = win::current_user().unwrap();
        let pipe = std::fs::OpenOptions::new().read(true).open(PIPE).unwrap();
        let expected = win::Descriptor::parse(&format!("D:P(A;;FA;;;SY)(A;;0x12008b;;;{me})"))
            .unwrap()
            .access()
            .unwrap();
        assert_eq!(win::access_of(&pipe).unwrap(), expected);
        drop(pipe);
        let state = state_dir().unwrap();
        let owner = win::owner_of(&state).unwrap();
        assert!(
            owner == win::SYSTEM || owner == win::ADMINISTRATORS,
            "{owner}"
        );
        assert!(installed().unwrap().is_file());
        assert!(Client::connect_checked(Path::new(PIPE), |_| Err("refused".to_string())).is_err());
        assert_eq!(client.hello().unwrap(), crate::protocol::VERSION);
    }

    #[test]
    #[ignore = "needs The Cabinet's helper installed as a service on this machine; it edits the hosts file"]
    fn the_installed_helper_redirects_the_test_name_while_a_caller_holds_it() {
        use crate::protocol::{Change, Scope};
        use std::net::{Ipv4Addr, ToSocketAddrs};
        const OWNER: &str = "0123456789abcdef/test";
        let redirect = |names: &[&str]| Change::Hosts {
            names: names.iter().map(|name| name.to_string()).collect(),
            target: Ipv4Addr::LOCALHOST,
        };
        let path = hosts().unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let resolves = || {
            ("cabinet-test.invalid", 0)
                .to_socket_addrs()
                .map(|mut found| found.any(|address| address.ip() == Ipv4Addr::LOCALHOST))
                .unwrap_or(false)
        };
        let eventually = |what: &str, check: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !check() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        assert!(!resolves(), "the test name resolves before the session");

        let mut client = connect().unwrap();
        client
            .apply(OWNER, Scope::Session, &redirect(&["cabinet-test.invalid"]))
            .unwrap();
        let during = std::fs::read_to_string(&path).unwrap();
        let end = if before.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        assert!(
            during.contains(&format!("{end}127.0.0.1 cabinet-test.invalid{end}")),
            "{during:?}"
        );
        eventually("the test name to resolve", &resolves);
        for refused in [&["api.example-game.net"][..], &["localhost"][..]] {
            client
                .apply(OWNER, Scope::Session, &redirect(refused))
                .unwrap_err();
        }
        assert_eq!(client.release().unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        eventually("the test name to stop resolving", &|| !resolves());

        let mut crashed = connect().unwrap();
        crashed
            .apply(OWNER, Scope::Session, &redirect(&["cabinet-test.invalid"]))
            .unwrap();
        assert_ne!(std::fs::read_to_string(&path).unwrap(), before);
        drop(crashed);
        eventually("a crashed caller's redirect to be undone", &|| {
            std::fs::read_to_string(&path).unwrap() == before
        });
        assert!(client.status().unwrap().is_empty());
    }
}
