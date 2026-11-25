//! Hyprland compositor IPC: socket discovery, dispatchers and batched commands.

use super::*;

pub(crate) fn hyprland_socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is unset")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is unset")?;
    if signature.is_empty()
        || !signature
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("HYPRLAND_INSTANCE_SIGNATURE is not a valid instance name".into());
    }
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock"))
}

pub(crate) fn hyprland_ipc(
    request: &str,
    max_bytes: usize,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    if request.is_empty() || request.len() > 64 * 1024 || request.contains('\0') {
        return Err("Hyprland IPC request is empty or exceeds 64 KiB".into());
    }
    let path = hyprland_socket_path()?;
    let mut socket = UnixStream::connect(&path)
        .map_err(|error| format!("connecting to Hyprland IPC at {}: {error}", path.display()))?;
    socket
        .set_read_timeout(Some(deadline))
        .map_err(|error| format!("setting Hyprland IPC read timeout: {error}"))?;
    socket
        .set_write_timeout(Some(deadline))
        .map_err(|error| format!("setting Hyprland IPC write timeout: {error}"))?;
    socket
        .write_all(request.as_bytes())
        .map_err(|error| format!("sending Hyprland IPC request: {error}"))?;
    socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("finishing Hyprland IPC request: {error}"))?;
    let mut response = Vec::with_capacity(max_bytes.min(64 * 1024));
    (&mut socket)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut response)
        .map_err(|error| format!("reading Hyprland IPC response: {error}"))?;
    if response.len() > max_bytes {
        return Err(format!("Hyprland IPC response exceeds {max_bytes} bytes"));
    }
    Ok(response)
}

pub(crate) fn hyprland_json(
    command: &str,
    max_bytes: usize,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    hyprland_ipc(&format!("j/{command}"), max_bytes, deadline)
}

pub(crate) fn hyprland_response_error(response: &[u8]) -> bool {
    let response = String::from_utf8_lossy(response);
    let response = response.trim_start().to_ascii_lowercase();
    response.is_empty()
        || response.starts_with("err")
        || response.starts_with("error:")
        || response == "unknown request"
}

pub(crate) fn set_hyprland_cursor_invisible(invisible: bool) -> Result<(), String> {
    let response = hyprland_ipc(
        &format!("eval hl.config({{ [\"cursor.invisible\"] = {invisible} }})"),
        64 * 1024,
        Duration::from_secs(3),
    )?;
    let response = String::from_utf8_lossy(&response);
    if response.trim() != "ok" {
        return Err(format!(
            "Hyprland rejected cursor.invisible update: {}",
            response.trim()
        ));
    }
    Ok(())
}

pub(crate) fn hypr(command: &str, args: &[&str]) -> Result<(), String> {
    let request = std::iter::once(command)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match hyprland_ipc(&request, 64 * 1024, Duration::from_secs(3)) {
        Ok(response) if response.is_empty() => {
            Err("Hyprland returned no action result; observe before retrying".into())
        }
        Ok(response) if !hyprland_response_error(&response) => Ok(()),
        Ok(response) if command == "dispatch" => {
            if let Some(lua) = hypr_lua_input(args) {
                let response =
                    hyprland_ipc(&format!("eval {lua}"), 64 * 1024, Duration::from_secs(3))?;
                if !hyprland_response_error(&response) {
                    return Ok(());
                }
                return Err(format!(
                    "Hyprland rejected action: {}",
                    String::from_utf8_lossy(&response).trim()
                ));
            }
            Err(format!(
                "Hyprland rejected action: {}",
                String::from_utf8_lossy(&response).trim()
            ))
        }
        Ok(response) => Err(format!(
            "Hyprland rejected action: {}",
            String::from_utf8_lossy(&response).trim()
        )),
        Err(error) => Err(format!("Hyprland action failed: {error}")),
    }
}

pub(crate) fn hypr_lua_input(args: &[&str]) -> Option<String> {
    match args {
        ["movecursor", coordinates] => {
            let mut values = coordinates.split_whitespace();
            let x: i32 = values.next()?.parse().ok()?;
            let y: i32 = values.next()?.parse().ok()?;
            if values.next().is_some() {
                return None;
            }
            Some(format!(
                "hl.dispatch(hl.dsp.cursor.move({{ x = {x}, y = {y} }}))"
            ))
        }
        ["sendkeystate", key_state] => {
            let fields: Vec<&str> = key_state.split(',').collect();
            if fields.len() != 4
                || !fields[0].is_empty()
                || fields[3] != "activewindow"
                || !matches!(fields[2], "down" | "up")
            {
                return None;
            }
            let key = serde_json::to_string(fields[1]).ok()?;
            Some(format!(
                "hl.dispatch(hl.dsp.send_key_state({{ mods = \"\", key = {key}, state = \"{}\", window = \"activewindow\" }}))",
                fields[2]
            ))
        }
        ["sendshortcut", shortcut] => {
            let fields: Vec<&str> = shortcut.split(',').collect();
            if fields.len() != 3 || fields[2] != "activewindow" {
                return None;
            }
            let mods = serde_json::to_string(fields[0]).ok()?;
            let key = serde_json::to_string(fields[1]).ok()?;
            Some(format!(
                "hl.dispatch(hl.dsp.send_shortcut({{ mods = {mods}, key = {key}, window = \"activewindow\" }}))"
            ))
        }
        _ => None,
    }
}

pub(crate) fn hypr_batch(commands: &[String]) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    for chunk in hypr_batch_chunks(commands)? {
        let batch = chunk
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(";");
        let response = match hyprland_ipc(
            &format!("[[BATCH]]{batch}"),
            1024 * 1024,
            Duration::from_secs(10),
        ) {
            Ok(response) => response,
            Err(error) => {
                return Err(format!(
                    "Hyprland batch result is unknown; observe before retrying: {error}"
                ));
            }
        };
        if response.is_empty() {
            return Err("Hyprland returned no batch result; observe before retrying".into());
        }
        let response_text = String::from_utf8_lossy(&response);
        let replies = response_text
            .split("\n\n\n")
            .map(str::trim)
            .collect::<Vec<_>>();
        if replies.len() != chunk.len() {
            return Err(format!(
                "Hyprland returned {} replies for {} input commands; batch result is unknown, observe before retrying: {}",
                replies.len(),
                chunk.len(),
                response_text.trim()
            ));
        }
        let rejected = replies
            .iter()
            .filter(|reply| hyprland_response_error(reply.as_bytes()))
            .count();
        if rejected == 0 {
            continue;
        }
        if rejected != replies.len() {
            return Err(format!(
                "Hyprland partially applied the input batch; observe before retrying: {}",
                response_text.trim()
            ));
        }
        if let Some(lua) = hypr_lua_batch(chunk) {
            let response =
                hyprland_ipc(&format!("eval {lua}"), 1024 * 1024, Duration::from_secs(10))?;
            if hyprland_response_error(&response) {
                return Err(format!(
                    "Hyprland rejected batch input: {}",
                    String::from_utf8_lossy(&response).trim()
                ));
            }
            continue;
        }
        return Err(format!(
            "Hyprland rejected batch input: {}",
            response_text.trim()
        ));
    }
    Ok(())
}

pub(crate) fn hypr_batch_chunks(commands: &[String]) -> Result<Vec<&[String]>, String> {
    const MAX_BATCH_BYTES: usize = 64 * 1024;
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, command) in commands.iter().enumerate() {
        if command.len() > MAX_BATCH_BYTES {
            return Err("Hyprland input command exceeds the batch size limit".into());
        }
        let separator = usize::from(index > start);
        if bytes + separator + command.len() > MAX_BATCH_BYTES {
            chunks.push(&commands[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += usize::from(index > start) + command.len();
    }
    if start < commands.len() {
        chunks.push(&commands[start..]);
    }
    Ok(chunks)
}

pub(crate) fn hypr_lua_batch(commands: &[String]) -> Option<String> {
    commands
        .iter()
        .map(|command| {
            let command = command.strip_prefix("dispatch ")?;
            let (dispatcher, argument) = command.split_once(' ')?;
            hypr_lua_input(&[dispatcher, argument])
        })
        .collect::<Option<Vec<_>>>()
        .map(|commands| commands.join(";"))
}

#[link(name = "dl")]
unsafe extern "C" {
    pub(crate) fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    pub(crate) fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    pub(crate) fn dlclose(handle: *mut c_void) -> c_int;
}

pub(crate) struct DynamicLibrary(NonNull<c_void>);
impl DynamicLibrary {
    pub(crate) fn open(name: &CStr) -> Result<Self, String> {
        let handle = unsafe { dlopen(name.as_ptr(), 2) };
        NonNull::new(handle)
            .map(Self)
            .ok_or_else(|| format!("could not load {}", name.to_string_lossy()))
    }

    pub(crate) fn symbol<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        let symbol = unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) };
        if symbol.is_null() {
            return Err(format!("missing symbol {}", name.to_string_lossy()));
        }
        Ok(unsafe { std::mem::transmute_copy(&symbol) })
    }
}
impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.0.as_ptr());
        }
    }
}
