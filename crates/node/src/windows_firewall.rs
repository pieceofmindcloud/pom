//! Windows Firewall integration for the four sockets used by a node.
//!
//! The node normally runs unelevated. The parent records the request before
//! starting a short-lived copy of itself with the `runas` verb, so declining
//! UAC cannot make startup or the network leave action depend on elevation.

use crate::auth::ConnectionAuth;
use crate::db::PomDb;
use crate::gateway::GatewayState;
use anyhow::{anyhow, Result};
use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::{
    net::{TcpListener, UdpSocket},
    sync::Arc,
};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum SetupDecision {
    Granted,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SetupRecord {
    pub p2p_port: u16,
    #[serde(default)]
    pub attempted: bool,
    pub decision: Option<SetupDecision>,
    pub last_error: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum FirewallProtocol {
    Udp,
    Tcp,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum FirewallRuleKind {
    Discovery,
    InferenceQuic,
    GatewayQuic,
    Libp2p,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct FirewallRuleSpec {
    pub kind: FirewallRuleKind,
    pub protocol: FirewallProtocol,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirewallRuleStatus {
    pub kind: FirewallRuleKind,
    pub protocol: FirewallProtocol,
    pub port: u16,
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirewallStatus {
    pub supported: bool,
    pub p2p_port: u16,
    pub rules: Vec<FirewallRuleStatus>,
    pub all_allowed: bool,
    pub automatic_attempted: bool,
    pub can_retry: bool,
    pub last_error: Option<String>,
    pub in_progress: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupFirewallCheck {
    pub supported: bool,
    pub allowed: bool,
}

pub fn desired_rules(p2p_port: u16) -> Result<Vec<FirewallRuleSpec>, String> {
    let inference = p2p_port
        .checked_add(1000)
        .ok_or_else(|| "a porta QUIC de inferência ultrapassa 65535".to_string())?;
    let gateway = p2p_port
        .checked_add(2000)
        .ok_or_else(|| "a porta QUIC do gateway ultrapassa 65535".to_string())?;
    let libp2p = p2p_port
        .checked_add(10000)
        .ok_or_else(|| "a porta TCP do libp2p ultrapassa 65535".to_string())?;
    Ok(vec![
        FirewallRuleSpec {
            kind: FirewallRuleKind::Discovery,
            protocol: FirewallProtocol::Udp,
            port: p2p_port,
        },
        FirewallRuleSpec {
            kind: FirewallRuleKind::InferenceQuic,
            protocol: FirewallProtocol::Udp,
            port: inference,
        },
        FirewallRuleSpec {
            kind: FirewallRuleKind::GatewayQuic,
            protocol: FirewallProtocol::Udp,
            port: gateway,
        },
        FirewallRuleSpec {
            kind: FirewallRuleKind::Libp2p,
            protocol: FirewallProtocol::Tcp,
            port: libp2p,
        },
    ])
}

/// Check the ports while setup is still before the node's network listeners.
/// Windows can inspect firewall rules directly; other platforms verify that
/// the POM ports are locally available for binding.
pub fn setup_check(database: &PomDb, p2p_port: u16) -> SetupFirewallCheck {
    let status = status_for(database, p2p_port);
    if status.supported {
        return SetupFirewallCheck {
            supported: true,
            allowed: status.all_allowed,
        };
    }

    let allowed = desired_rules(p2p_port)
        .map(|rules| rules.iter().all(local_port_available))
        .unwrap_or(false);
    SetupFirewallCheck {
        supported: false,
        allowed,
    }
}

fn local_port_available(rule: &FirewallRuleSpec) -> bool {
    match rule.protocol {
        FirewallProtocol::Udp => UdpSocket::bind(("0.0.0.0", rule.port)).is_ok(),
        FirewallProtocol::Tcp => TcpListener::bind(("0.0.0.0", rule.port)).is_ok(),
    }
}

#[cfg(any(windows, test))]
pub(crate) fn should_auto_request(previous: Option<&SetupRecord>, retry: bool) -> bool {
    retry || previous.is_none_or(|record| !record.attempted)
}

pub fn spawn_startup_check(database: Arc<PomDb>, p2p_port: u16) {
    #[cfg(windows)]
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || startup_check(&database, p2p_port)).await;
        match result {
            Ok(Ok(())) => tracing::debug!(p2p_port, "verificação do firewall do Windows concluída"),
            Ok(Err(error)) => tracing::warn!(
                p2p_port,
                "não foi possível verificar o firewall do Windows: {error:#}"
            ),
            Err(error) => tracing::warn!(
                p2p_port,
                "verificação do firewall do Windows cancelada: {error}"
            ),
        }
    });

    #[cfg(not(windows))]
    let _ = (database, p2p_port);
}

pub async fn status(_auth: ConnectionAuth, State(state): State<GatewayState>) -> impl IntoResponse {
    Json(status_for(&state.database, state.network.bind_addr.port()))
}

pub async fn retry(_auth: ConnectionAuth, State(state): State<GatewayState>) -> impl IntoResponse {
    let port = state.network.bind_addr.port();
    let database = state.database.clone();
    let result = tokio::task::spawn_blocking(move || request_elevation(&database, port)).await;
    match result {
        Ok(Ok(())) => Json(status_for(&state.database, port)).into_response(),
        Ok(Err(error)) => {
            tracing::warn!(
                p2p_port = port,
                "permissão do firewall recusada ou falhou: {error:#}"
            );
            Json(status_for(&state.database, port)).into_response()
        }
        Err(error) => {
            tracing::warn!(
                p2p_port = port,
                "pedido de permissão do firewall cancelado: {error}"
            );
            Json(status_for(&state.database, port)).into_response()
        }
    }
}

fn status_for(database: &PomDb, p2p_port: u16) -> FirewallStatus {
    let record = database.windows_firewall_setup().ok().flatten();
    let desired = desired_rules(p2p_port);
    let rules = desired
        .as_ref()
        .map(|rules| {
            rules
                .iter()
                .map(|rule| FirewallRuleStatus {
                    kind: rule.kind,
                    protocol: rule.protocol,
                    port: rule.port,
                    allowed: rules_allowed(p2p_port, rule).unwrap_or(false),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let all_allowed = desired.is_ok() && !rules.is_empty() && rules.iter().all(|rule| rule.allowed);
    let automatic_attempted = record
        .as_ref()
        .filter(|record| record.p2p_port == p2p_port)
        .is_some_and(|record| record.attempted);
    let can_retry = !all_allowed;
    FirewallStatus {
        supported: cfg!(windows),
        p2p_port,
        all_allowed,
        rules,
        automatic_attempted,
        can_retry,
        last_error: record
            .filter(|record| record.p2p_port == p2p_port)
            .and_then(|record| record.last_error),
        in_progress: false,
    }
}

fn rules_allowed(_p2p_port: u16, _rule: &FirewallRuleSpec) -> Result<bool, String> {
    #[cfg(windows)]
    {
        return platform::rules_allowed(_p2p_port, _rule).map_err(|error| error.to_string());
    }

    #[cfg(not(windows))]
    {
        Ok(true)
    }
}

#[cfg(windows)]
fn startup_check(database: &PomDb, p2p_port: u16) -> Result<()> {
    let rules = desired_rules(p2p_port).map_err(|error| anyhow!(error))?;
    if rules
        .iter()
        .all(|rule| rules_allowed(p2p_port, rule).unwrap_or(false))
    {
        database.save_windows_firewall_setup(&SetupRecord {
            p2p_port,
            attempted: true,
            decision: Some(SetupDecision::Granted),
            last_error: None,
        })?;
        return Ok(());
    }

    let previous = database
        .windows_firewall_setup()?
        .filter(|record| record.p2p_port == p2p_port);
    if !should_auto_request(previous.as_ref(), false) {
        return Ok(());
    }
    request_elevation(database, p2p_port)
}

fn request_elevation(database: &PomDb, p2p_port: u16) -> Result<()> {
    database.save_windows_firewall_setup(&SetupRecord {
        p2p_port,
        attempted: true,
        decision: None,
        last_error: None,
    })?;

    #[cfg(windows)]
    let result = launch_elevated_helper(p2p_port);
    #[cfg(not(windows))]
    let result: Result<()> = Err(anyhow!("o firewall automático só existe no Windows"));

    match result {
        Ok(()) => database.save_windows_firewall_setup(&SetupRecord {
            p2p_port,
            attempted: true,
            decision: Some(SetupDecision::Granted),
            last_error: None,
        }),
        Err(error) => {
            let message = error.to_string();
            database.save_windows_firewall_setup(&SetupRecord {
                p2p_port,
                attempted: true,
                decision: Some(SetupDecision::Declined),
                last_error: Some(message.clone()),
            })?;
            Err(anyhow!(message))
        }
    }
}

#[cfg(windows)]
pub(crate) fn run_elevated_helper(p2p_port: u16) -> Result<()> {
    apply_rules(p2p_port)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{BSTR, GUID, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_IP_PROTOCOL_TCP,
        NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_IN,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    const CLSID_NET_FW_POLICY2: GUID = GUID::from_u128(0xe2b3c97f6ae141ac817af6f92166d7dd);
    const CLSID_NET_FW_RULE: GUID = GUID::from_u128(0x2c5bc43e33694c33ab0cbe9469677af4);

    struct ComGuard;
    impl ComGuard {
        fn new() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                .ok()
                .map_err(|error| anyhow!("inicializar COM do firewall: {error}"))?;
            Ok(Self)
        }
    }
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn apply_rules(port: u16) -> Result<()> {
        let _com = ComGuard::new()?;
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&CLSID_NET_FW_POLICY2, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| anyhow!("abrir política do Windows Firewall: {error}"))?;
        let rules = unsafe { policy.Rules() }
            .map_err(|error| anyhow!("abrir regras do Windows Firewall: {error}"))?;
        let executable = std::env::current_exe()?.to_string_lossy().into_owned();
        for spec in desired_rules(port).map_err(|error| anyhow!(error))? {
            let name = rule_name(&spec);
            let rule = match unsafe { rules.Item(&BSTR::from(name.as_str())) } {
                Ok(rule) => rule,
                Err(_) => {
                    unsafe { CoCreateInstance(&CLSID_NET_FW_RULE, None, CLSCTX_INPROC_SERVER) }
                        .map_err(|error| anyhow!("criar regra {name}: {error}"))?
                }
            };
            configure_rule(&rule, &spec, &executable)?;
            if unsafe { rules.Item(&BSTR::from(name.as_str())) }.is_err() {
                unsafe { rules.Add(&rule) }
                    .map_err(|error| anyhow!("adicionar regra {name}: {error}"))?;
            }
        }
        Ok(())
    }

    fn configure_rule(rule: &INetFwRule, spec: &FirewallRuleSpec, executable: &str) -> Result<()> {
        let name = BSTR::from(rule_name(spec));
        let description = BSTR::from("Portas POM para descoberta, QUIC e libp2p");
        let application = BSTR::from(executable);
        let port = BSTR::from(spec.port.to_string());
        unsafe {
            rule.SetName(&name)?;
            rule.SetDescription(&description)?;
            rule.SetApplicationName(&application)?;
            rule.SetProtocol(match spec.protocol {
                FirewallProtocol::Udp => NET_FW_IP_PROTOCOL_UDP.0,
                FirewallProtocol::Tcp => NET_FW_IP_PROTOCOL_TCP.0,
            })?;
            rule.SetLocalPorts(&port)?;
            rule.SetDirection(NET_FW_RULE_DIR_IN)?;
            rule.SetEnabled(true.into())?;
            rule.SetProfiles(NET_FW_PROFILE2_ALL.0)?;
            rule.SetAction(NET_FW_ACTION_ALLOW)?;
        }
        Ok(())
    }

    pub(super) fn rules_allowed(port: u16, spec: &FirewallRuleSpec) -> Result<bool> {
        let _com = ComGuard::new()?;
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&CLSID_NET_FW_POLICY2, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| anyhow!("abrir política do Windows Firewall: {error}"))?;
        let rules = unsafe { policy.Rules() }
            .map_err(|error| anyhow!("abrir regras do Windows Firewall: {error}"))?;
        let rule = unsafe { rules.Item(&BSTR::from(rule_name(spec))) };
        let Ok(rule) = rule else { return Ok(false) };
        let expected_protocol = match spec.protocol {
            FirewallProtocol::Udp => NET_FW_IP_PROTOCOL_UDP.0,
            FirewallProtocol::Tcp => NET_FW_IP_PROTOCOL_TCP.0,
        };
        Ok(unsafe { rule.Enabled()?.as_bool() }
            && unsafe { rule.Direction()? } == NET_FW_RULE_DIR_IN
            && unsafe { rule.Action()? } == NET_FW_ACTION_ALLOW
            && unsafe { rule.Profiles()? } == NET_FW_PROFILE2_ALL.0
            && unsafe { rule.Protocol()? } == expected_protocol
            && unsafe { rule.LocalPorts()?.to_string() } == spec.port.to_string())
    }

    pub(super) fn launch_elevated_helper(port: u16) -> Result<()> {
        let executable = std::env::current_exe()?;
        let executable = wide(executable.as_os_str());
        let verb = wide("runas");
        let parameters = wide(format!("--windows-firewall-helper {port}"));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(executable.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: 0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut info) }
            .map_err(|error| anyhow!("solicitar elevação UAC: {error}"))?;
        let process = info.hProcess;
        if process.is_invalid() {
            return Err(anyhow!("o Windows não retornou o processo elevado"));
        }
        unsafe { WaitForSingleObject(process, INFINITE) };
        let mut exit_code = 1;
        unsafe { GetExitCodeProcess(process, &mut exit_code) }
            .map_err(|error| anyhow!("ler resultado do helper elevado: {error}"))?;
        unsafe { CloseHandle(process) }
            .map_err(|error| anyhow!("fechar helper elevado: {error}"))?;
        if exit_code == 0 {
            Ok(())
        } else {
            Err(anyhow!("helper elevado terminou com código {exit_code}"))
        }
    }

    fn rule_name(spec: &FirewallRuleSpec) -> String {
        format!(
            "POM {} {} {}",
            spec.kind.label(),
            spec.protocol.label(),
            spec.port
        )
    }

    fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    trait Labels {
        fn label(self) -> &'static str;
    }
    impl Labels for FirewallRuleKind {
        fn label(self) -> &'static str {
            match self {
                FirewallRuleKind::Discovery => "discovery",
                FirewallRuleKind::InferenceQuic => "inference-quic",
                FirewallRuleKind::GatewayQuic => "gateway-quic",
                FirewallRuleKind::Libp2p => "libp2p",
            }
        }
    }
    impl Labels for FirewallProtocol {
        fn label(self) -> &'static str {
            match self {
                FirewallProtocol::Udp => "UDP",
                FirewallProtocol::Tcp => "TCP",
            }
        }
    }
}

#[cfg(windows)]
use platform::{apply_rules, launch_elevated_helper};
