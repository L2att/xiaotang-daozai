//! 2.4G 无线鼠标电量（VK M3 Lite / 泰坦 F2）。
//!
//! 协议逆向自 VK M3 官方驱动：两个接收器共用 `VID:0x249A PID:0x5C2F`，
//! vendor 接口（MI_02，usage page 0xFFEF）收发 65 字节报告。
//!
//! - 查询帧：`[0]=0`（report id）、`[1]=0x10`、`[0x20]=sum([6..0x20])`
//! - 响应帧：`[0]=0x10 [1]=0 [4..8]=型号字符串 [12]=充电 [13]=电量 [14]=在线`
//!
//! 后台线程定期查询并推给界面：有读数时 60 秒一轮，没读数时 10 秒一轮
//! 以便鼠标唤醒后尽快补上。读数会缓存到磁盘，重启后先显示上次电量。

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 固定两个接收器，顺序与 `Battery` 数组一致。
pub const NAMES: [&str; 2] = ["VK M3 Lite", "泰坦 F2"];
/// 响应帧 `[4..8]` 的型号字符串，用来区分插在同一台电脑上的多个接收器。
const MODELS: [&str; 2] = ["M220", "M102"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Battery {
    pub level: Option<u8>,
    pub charging: bool,
}

impl Battery {
    pub fn summary(&self) -> String {
        match (self.level, self.charging) {
            (Some(level), true) => format!("充电中 {level}%"),
            (Some(level), false) => format!("{level}%"),
            (None, true) => "充电中".into(),
            (None, false) => "未连接/休眠".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub batteries: [Battery; 2],
    /// 两把鼠标都在线时沿用上次选择；有鼠标重新上线时切换到它。
    pub selected: usize,
}

impl State {
    pub fn current(&self) -> &Battery {
        &self.batteries[self.selected.min(NAMES.len() - 1)]
    }
    pub fn name(&self) -> &'static str {
        NAMES[self.selected.min(NAMES.len() - 1)]
    }
    /// 岛条悬停/设置页使用的一行摘要。
    pub fn summary(&self) -> String {
        format!("2.4G · {} · {}", self.name(), self.current().summary())
    }
    /// 新一轮没有读到电量时保留上次的值，读到新值再覆盖。
    pub fn merge(&mut self, fresh: State) {
        for (old, new) in self.batteries.iter_mut().zip(fresh.batteries) {
            if new.level.is_some() {
                *old = new;
            }
        }
        self.selected = fresh.selected;
    }
    fn has_data(&self) -> bool { self.batteries.iter().any(|battery| battery.level.is_some()) }
    /// 上次成功读到的电量；用于重启后先显示旧值，等到新数据再覆盖。
    pub fn load() -> State {
        let path = crate::model::config_dir().join("mouse-battery.json");
        std::fs::read_to_string(path).ok()
            .and_then(|text| serde_json::from_str::<State>(&text).ok())
            .filter(|state| state.selected < NAMES.len()
                && state.batteries.iter().all(|battery| battery.level.is_none_or(|level| level <= 100)))
            .unwrap_or_default()
    }
    fn save(&self) {
        let path = crate::model::config_dir().join("mouse-battery.json");
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(text) = serde_json::to_string(self) { let _ = std::fs::write(path, text); }
    }
}

pub struct Service {
    pub rx: std::sync::mpsc::Receiver<State>,
    stop: std::sync::mpsc::Sender<()>,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// 启动后台查询线程；服务被丢弃后线程在下一轮唤醒时退出。
pub fn spawn() -> Service {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (stop, stopped) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("daozai-mouse-battery".into()).spawn(move || {
        let mut last = State::load();
        let mut preferred = last.has_data().then_some(last.selected);
        loop {
            let previous = last;
            last.merge(query(&mut preferred));
            if last != previous { last.save(); }
            if matches!(tx.try_send(last), Err(std::sync::mpsc::TrySendError::Disconnected(_))) {
                return;
            }
            let wait = if last.has_data() { 60 } else { 10 };
            match stopped.recv_timeout(Duration::from_secs(wait)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
    }).ok();
    Service { rx, stop }
}

#[cfg(not(windows))]
fn query(_preferred: &mut Option<usize>) -> State {
    State::default()
}

/// HID 查询命令：65 字节优先，部分固件只接受 33 字节。
pub fn build_query(size: usize) -> Vec<u8> {
    let mut frame = vec![0u8; size.clamp(2, 65)];
    frame[0] = 0;
    frame[1] = 0x10;
    if frame.len() >= 0x21 {
        frame[0x20] = frame.get(6..0x20).map(|data| data.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))).unwrap_or(0);
    }
    frame
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub model: String,
    pub battery: u8,
    pub charging: u8,
    pub online: u8,
}

/// 解析 `0x10` 查询响应；不是查询响应时返回 `None`。
pub fn parse_response(frame: &[u8]) -> Option<Response> {
    let data = if frame.len() > 1 && frame[0] != 0x10 && frame[1] == 0x10 { &frame[1..] } else { frame };
    if data.len() < 15 || data[0] != 0x10 || data[1] != 0 {
        return None;
    }
    let model = std::str::from_utf8(&data[4..8]).ok()?.trim_matches(|c: char| c == '\0' || c == ' ').to_string();
    if model.is_empty() {
        return None;
    }
    Some(Response { model, battery: data[13], charging: data[12], online: data[14] })
}

fn device_index(model: &str) -> Option<usize> {
    let model = model.trim().to_ascii_uppercase();
    MODELS.iter().position(|known| *known == model)
}

/// 接收器原始状态 -> 界面电量：只有充电或在线时才认为数据有效。
fn normalize(response: &Response) -> Battery {
    let level = (response.battery <= 100).then_some(response.battery);
    if response.charging == 1 {
        Battery { level, charging: true }
    } else if response.online == 1 {
        Battery { level, charging: false }
    } else {
        Battery::default()
    }
}

#[cfg(windows)]
fn query(preferred: &mut Option<usize>) -> State {
    let responses = win::query_dongles();
    let mut state = State::default();
    let mut seen = [false; 2];
    for response in &responses {
        let Some(index) = device_index(&response.model) else { continue };
        if seen[index] {
            continue;
        }
        seen[index] = true;
        state.batteries[index] = normalize(response);
    }
    state.selected = select(&responses, preferred);
    state
}

/// 两把鼠标只有一把在线时选在线的那把；都在线或都休眠时沿用上次选择。
fn select(responses: &[Response], preferred: &mut Option<usize>) -> usize {
    let active: Vec<usize> = responses.iter()
        .filter(|response| response.charging == 1 || response.online == 1)
        .filter_map(|response| device_index(&response.model))
        .collect();
    if let Some(current) = *preferred {
        if active.contains(&current) {
            return current;
        }
        if let Some(&next) = active.first() {
            *preferred = Some(next);
            return next;
        }
        return current;
    }
    let chosen = active.first().copied().unwrap_or_else(|| {
        responses.iter().filter_map(|response| device_index(&response.model)).min().unwrap_or(0)
    });
    *preferred = Some(chosen);
    chosen
}

#[cfg(windows)]
mod win {
    use super::{build_query, parse_response, Response};
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows::Win32::Devices::HumanInterfaceDevice::*;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
    use std::time::{Duration, Instant};

    /// 关闭句柄的 RAII 包装：查询过程中任何提前返回都不会泄漏。
    struct Owned(HANDLE);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }

    /// 枚举所有 VID/PID 匹配的 HID 接口，只取 vendor 接口（MI_02）。
    fn vendor_paths() -> Vec<Vec<u16>> {
        let mut paths = Vec::new();
        unsafe {
            let guid = HidD_GetHidGuid();
            let Ok(set) = SetupDiGetClassDevsW(Some(&guid as *const _), PCWSTR::null(), HWND::default(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) else {
                return paths;
            };
            for index in 0.. {
                let mut interface = SP_DEVICE_INTERFACE_DATA {
                    cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInterfaces(set, None, &guid, index, &mut interface).is_err() {
                    break;
                }
                let mut need = 0u32;
                let _ = SetupDiGetDeviceInterfaceDetailW(set, &interface, None, 0, Some(&mut need), None);
                if need < std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
                    continue;
                }
                // 详情结构尾部是变长路径，按 8 字节对齐分配。
                let mut raw = vec![0u64; (need as usize + 7) / 8];
                let detail = raw.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(set, &interface, Some(detail), need, None, None).is_err() {
                    continue;
                }
                let path = detail_path(detail);
                let lower = path.to_ascii_lowercase();
                if !lower.contains("vid_249a") || !lower.contains("pid_5c2f") {
                    continue;
                }
                // 复合设备的 vendor 接口是 MI_02；单接口接收器路径里没有 mi_。
                if lower.contains("mi_02") || !lower.contains("mi_") {
                    paths.push(to_wide(&path));
                }
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
        paths
    }

    unsafe fn detail_path(detail: *const SP_DEVICE_INTERFACE_DETAIL_DATA_W) -> String {
        let ptr = (*detail).DevicePath.as_ptr();
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }

    fn to_wide(path: &str) -> Vec<u16> {
        path.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 查询全部接收器，返回每个接收器的查询响应。
    pub(super) fn query_dongles() -> Vec<Response> {
        vendor_paths().into_iter().filter_map(|path| query_path(&path)).collect()
    }

    /// 读取 HID 报告长度；失败时按 65 字节处理。
    unsafe fn caps(handle: HANDLE) -> Option<HIDP_CAPS> {
        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        if !HidD_GetPreparsedData(handle, &mut preparsed).as_bool() {
            return None;
        }
        let mut caps = HIDP_CAPS::default();
        let status = HidP_GetCaps(preparsed, &mut caps);
        let _ = HidD_FreePreparsedData(preparsed);
        (status.0 >= 0).then_some(caps)
    }

    fn query_path(path: &[u16]) -> Option<Response> {
        unsafe {
            let handle = Owned(CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                HANDLE::default(),
            ).ok()?);
            let _ = HidD_SetNumInputBuffers(handle.0, 64);
            let event = Owned(CreateEventW(None, true, false, PCWSTR::null()).ok()?);

            // Windows HID 写入长度必须等于声明的输出报告长度。hidapi 对不带编号的
            // 报告会去掉首字节 report id，这里按同样的规则裁剪后再写。
            let caps = caps(handle.0);
            let output_len = caps.map(|caps| caps.OutputReportByteLength as usize).filter(|len| *len > 0).unwrap_or(65);
            let input_len = caps.map(|caps| caps.InputReportByteLength as usize).filter(|len| *len > 0).unwrap_or(65).max(65);
            let mut written = false;
            for size in [65usize, 33] {
                if size < 2 {
                    continue;
                }
                let frame = build_query(size);
                let payload = if output_len == 65 || output_len == size {
                    frame.as_slice()
                } else if output_len + 1 == size {
                    &frame[1..]
                } else {
                    continue;
                };
                if write(handle.0, event.0, payload) {
                    written = true;
                    break;
                }
            }
            if !written {
                return None;
            }

            // 响应通常 10ms 内返回；最多等 1.5 秒，避免两把鼠标串行拖慢刷新。
            let deadline = Instant::now() + Duration::from_millis(1500);
            let mut buffer = vec![0u8; input_len];
            while Instant::now() < deadline {
                let Some(read) = read(handle.0, event.0, &mut buffer) else { continue };
                if let Some(response) = parse_response(&buffer[..read]) {
                    return Some(response);
                }
            }
            None
        }
    }

    unsafe fn write(handle: HANDLE, event: HANDLE, data: &[u8]) -> bool {
        let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
        let _ = ResetEvent(event);
        let _ = WriteFile(handle, Some(data), None, Some(&mut overlapped));
        finish(handle, event, &overlapped, 500).is_some()
    }

    unsafe fn read(handle: HANDLE, event: HANDLE, buffer: &mut [u8]) -> Option<usize> {
        let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
        let _ = ResetEvent(event);
        let _ = ReadFile(handle, Some(buffer), None, Some(&mut overlapped));
        finish(handle, event, &overlapped, 150).map(|bytes| bytes as usize)
    }

    unsafe fn finish(handle: HANDLE, event: HANDLE, overlapped: &OVERLAPPED, timeout: u32) -> Option<u32> {
        if WaitForSingleObject(event, timeout) != WAIT_OBJECT_0 {
            let _ = CancelIoEx(handle, Some(overlapped as *const OVERLAPPED));
            let _ = WaitForSingleObject(event, 100);
            return None;
        }
        let mut transferred = 0u32;
        GetOverlappedResult(handle, overlapped, &mut transferred, false).ok().map(|_| transferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_frame_carries_report_id_command_and_checksum() {
        let frame = build_query(65);
        assert_eq!(frame.len(), 65);
        assert_eq!(&frame[..2], &[0, 0x10]);
        assert_eq!(frame[0x20], 0);
        let frame = build_query(33);
        assert_eq!(frame.len(), 33);
        assert_eq!(&frame[..2], &[0, 0x10]);
    }

    #[test]
    fn parses_response_and_skips_other_reports() {
        let mut frame = [0u8; 65];
        frame[0] = 0x10;
        frame[1] = 0;
        frame[4..8].copy_from_slice(b"M220");
        frame[12] = 1;
        frame[13] = 57;
        frame[14] = 1;
        let response = parse_response(&frame).expect("应识别查询响应");
        assert_eq!(response.model, "M220");
        assert_eq!((response.battery, response.charging, response.online), (57, 1, 1));
        assert_eq!(device_index(&response.model), Some(0));
        assert_eq!(device_index("M102"), Some(1));
        assert_eq!(device_index("XXXX"), None);
        assert_eq!(normalize(&response), Battery { level: Some(57), charging: true });

        // hidapi 风格：首字节是 report id 时先剥掉。
        let mut prefixed = [0u8; 66];
        prefixed[1..].copy_from_slice(&frame);
        assert_eq!(parse_response(&prefixed).map(|r| r.model), Some("M220".into()));

        assert_eq!(parse_response(&[0xC0, 1, 80, 0]), None);
    }

    #[test]
    fn offline_dongle_reports_no_level_and_online_keeps_it() {
        let mut frame = [0u8; 65];
        frame[0] = 0x10;
        frame[4..8].copy_from_slice(b"M102");
        frame[13] = 71;
        frame[14] = 0;
        let response = parse_response(&frame).unwrap();
        assert_eq!(normalize(&response), Battery::default());
        frame[14] = 1;
        let response = parse_response(&frame).unwrap();
        assert_eq!(normalize(&response), Battery { level: Some(71), charging: false });
    }

    fn response(model:&[u8;4],battery:u8,charging:u8,online:u8)->Response {
        Response{model:std::str::from_utf8(model).unwrap().into(),battery,charging,online}
    }

    #[test]
    fn selection_prefers_the_mouse_in_use_and_sticks_while_both_are_online() {
        let vk=response(b"M220",57,0,0);
        let f2=response(b"M102",81,0,1);
        let mut preferred=None;
        assert_eq!(select(&[vk.clone(),f2.clone()],&mut preferred),1,"F2 在线时优先 F2");
        let mut both_online=f2.clone();
        both_online.online=1;
        assert_eq!(select(&[vk.clone(),both_online],&mut preferred),1,"两把都在线时沿用上次使用的那把");
        let mut vk_online=vk.clone();
        vk_online.online=1;
        let mut f2_sleep=f2.clone();
        f2_sleep.online=0;
        assert_eq!(select(&[vk_online,f2_sleep.clone()],&mut preferred),0,"F2 休眠后切到在线的 VK");
        assert_eq!(select(&[vk,f2_sleep],&mut preferred),0,"都休眠时保留上次使用的 VK");
    }

    #[test]
    fn merge_keeps_last_reading_until_new_data_arrives() {
        let mut state=State{batteries:[Battery{level:Some(53),charging:false},Battery{level:Some(81),charging:false}],selected:0};
        state.merge(State{batteries:[Battery::default(),Battery{level:Some(90),charging:true}],selected:1});
        assert_eq!(state.batteries[0].level,Some(53),"读不到时保留上次电量");
        assert_eq!(state.batteries[1],Battery{level:Some(90),charging:true},"读到新值时更新");
        assert_eq!(state.selected,1);
        state.merge(State::default());
        assert_eq!(state.batteries[0].level,Some(53));
        assert_eq!(state.batteries[1].level,Some(90));
    }

    #[test]
    fn state_summary_uses_the_selected_receiver() {
        let state=State{batteries:[Battery{level:Some(57),charging:false},Battery{level:Some(81),charging:true}],selected:1};
        assert_eq!(state.summary(),"2.4G · 泰坦 F2 · 充电中 81%");
    }
}
