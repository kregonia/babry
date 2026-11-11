//! 合成器检测、直接 IPC 和 Wayland 输出元数据。
use crate::core::RectPx;
use serde_json::Value;
use smithay_client_toolkit::reexports::client::{
    Connection, EventQueue, QueueHandle,
    globals::registry_queue_init,
    protocol::wl_output::{Transform, WlOutput},
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Niri,
    Hyprland,
    Kde,
}
impl Backend {
    pub fn detect() -> Result<Self, String> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_lowercase();
        if desktop.contains("kde") || desktop.contains("plasma") {
            return Ok(Self::Kde);
        }
        if desktop.contains("niri") {
            return Ok(Self::Niri);
        }
        if desktop.contains("hypr") {
            return Ok(Self::Hyprland);
        }
        if std::env::var_os("NIRI_SOCKET").is_some() {
            Ok(Self::Niri)
        } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            Ok(Self::Hyprland)
        } else if std::env::var_os("KDE_FULL_SESSION").is_some() {
            Ok(Self::Kde)
        } else {
            Err("当前支持 niri、Hyprland 和 KDE Plasma 6.8+".into())
        }
    }
}
#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub focused: bool,
}
pub fn select_output(outputs: &[OutputInfo], name: Option<&str>) -> Result<OutputInfo, String> {
    if let Some(name) = name {
        outputs
            .iter()
            .find(|o| o.name == name)
            .cloned()
            .ok_or_else(|| format!("显示器 {name} 不存在或未启用"))
    } else {
        outputs
            .iter()
            .find(|o| o.focused)
            .or_else(|| outputs.first())
            .cloned()
            .ok_or("没有启用的显示器".into())
    }
}
fn niri_query(request: &str) -> Result<Value, String> {
    let path = std::env::var_os("NIRI_SOCKET").ok_or("NIRI_SOCKET 未设置")?;
    let mut socket = UnixStream::connect(path).map_err(|e| format!("连接 niri IPC 失败: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    writeln!(socket, "\"{request}\"").map_err(|e| e.to_string())?;
    let mut data = String::new();
    BufReader::new(socket)
        .take(4 * 1024 * 1024)
        .read_line(&mut data)
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    value
        .get("Ok")
        .cloned()
        .ok_or_else(|| format!("niri IPC 返回错误: {value}"))
}
fn hypr_query(request: &str) -> Result<Value, String> {
    use std::io::Read;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR 未设置")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or("Hyprland 实例未设置")?;
    let path = PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");
    let mut socket =
        UnixStream::connect(path).map_err(|e| format!("连接 Hyprland IPC 失败: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(format!("j/{request}").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    socket
        .take(4 * 1024 * 1024)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}
pub fn focused_name() -> Option<String> {
    match Backend::detect().ok()? {
        Backend::Niri => niri_query("FocusedOutput")
            .ok()?
            .get("FocusedOutput")?
            .get("name")?
            .as_str()
            .map(str::to_owned),
        Backend::Hyprland => hypr_query("monitors")
            .ok()?
            .as_array()?
            .iter()
            .find(|o| o["focused"] == true)?
            .get("name")?
            .as_str()
            .map(str::to_owned),
        Backend::Kde => crate::kde::active_output().ok(),
    }
}
struct Probe {
    registry: RegistryState,
    outputs: OutputState,
}
impl OutputHandler for Probe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}
delegate_output!(Probe);
delegate_registry!(Probe);
impl ProvidesRegistryState for Probe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
/// 在当前连接上按名称绑定输出，并持有事件队列直到表面退出。
/// 编辑器和贴图必须使用同一个目标输出，不能依赖合成器的默认输出选择。
pub struct BoundOutput {
    pub output: WlOutput,
    _queue: EventQueue<Probe>,
    _state: Probe,
}

pub fn bind_output(connection: &Connection, name: &str) -> Result<BoundOutput, String> {
    let (globals, mut queue) =
        registry_queue_init::<Probe>(connection).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = Probe {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
    };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let output = state
        .outputs
        .outputs()
        .find(|output| {
            state
                .outputs
                .info(output)
                .and_then(|info| info.name)
                .as_deref()
                == Some(name)
        })
        .ok_or_else(|| format!("显示器 {name} 不存在或未启用"))?;
    Ok(BoundOutput {
        output,
        _queue: queue,
        _state: state,
    })
}

pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    if crate::clipboard::x11_session() {
        return crate::x11::outputs();
    }
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = Probe {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
    };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let focused = focused_name();
    Ok(state
        .outputs
        .outputs()
        .filter_map(|o| {
            let i = state.outputs.info(&o)?;
            let name = i.name?;
            let (x, y) = i.logical_position?;
            let (w, h) = i.logical_size?;
            if w <= 0 || h <= 0 {
                return None;
            }
            let scale = i
                .modes
                .iter()
                .find(|m| m.current)
                .map(|m| {
                    let rotated = matches!(
                        i.transform,
                        Transform::_90
                            | Transform::_270
                            | Transform::Flipped90
                            | Transform::Flipped270
                    );
                    let (pw, ph) = if rotated {
                        (m.dimensions.1, m.dimensions.0)
                    } else {
                        m.dimensions
                    };
                    (pw as f32 / w as f32 + ph as f32 / h as f32) * 0.5
                })
                .unwrap_or(i.scale_factor as f32);
            Some(OutputInfo {
                focused: focused.as_ref() == Some(&name),
                name,
                x,
                y,
                width: w as u32,
                height: h as u32,
                scale,
            })
        })
        .collect())
}
/// niri 26.04 的平铺窗口不返回 `tile_pos_in_workspace_view`（只有浮动窗口和新版
/// niri 才返回），因此必须用列/行索引、tile 尺寸和焦点列居中重建可见几何。
#[derive(Clone, Copy, Debug)]
struct NiriTile {
    column: usize,
    row: usize,
    width: f64,
    height: f64,
    /// 窗口内容相对 tile 的偏移和尺寸，用于浮动/全屏窗口的内部内容区域。
    offset: (f64, f64),
    window: (f64, f64),
    exact: Option<(f64, f64)>,
    focused: bool,
}

fn place_niri_tiles(tiles: &[NiriTile], view: (f64, f64), gap: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut placed = vec![(0.0, 0.0, 0.0, 0.0); tiles.len()];
    if tiles.is_empty() {
        return placed;
    }
    let mut complete = true;
    for (slot, tile) in placed.iter_mut().zip(tiles) {
        match tile.exact {
            Some((x, y)) => *slot = (x, y, tile.width, tile.height),
            None => complete = false,
        }
    }
    if complete {
        return placed;
    }
    // 浮动窗口有精确位置，只重建没有位置的平铺窗口；锚点必须是平铺焦点窗口。
    let anchor = tiles
        .iter()
        .position(|tile| tile.focused && tile.exact.is_none())
        .or_else(|| tiles.iter().position(|tile| tile.exact.is_none()))
        .unwrap_or(0);
    let mut columns: Vec<(usize, f64)> = Vec::new();
    for tile in tiles.iter().filter(|tile| tile.exact.is_none()) {
        match columns
            .iter_mut()
            .find(|(column, _)| *column == tile.column)
        {
            Some((_, width)) => *width = width.max(tile.width),
            None => columns.push((tile.column, tile.width)),
        }
    }
    columns.sort_by_key(|(column, _)| *column);
    let column_width = |column: usize| {
        columns
            .iter()
            .find(|(c, _)| *c == column)
            .map(|(_, width)| *width)
            .unwrap_or(0.0)
    };
    // 最高的一列通常占满工作区；用它反推顶栏占掉的逻辑像素。
    let span = columns
        .iter()
        .map(|(column, _)| {
            let mut rows: Vec<&NiriTile> = tiles
                .iter()
                .filter(|tile| tile.exact.is_none() && tile.column == *column)
                .collect();
            rows.sort_by_key(|tile| tile.row);
            rows.iter().map(|tile| tile.height).sum::<f64>()
                + gap * rows.len().saturating_sub(1) as f64
        })
        .fold(0.0, f64::max);
    let working = (span + 2.0 * gap).min(view.1);
    let top = view.1 - working;
    let focus_column = tiles[anchor].column;
    let focus_width = column_width(focus_column);
    let focus_x = ((view.0 - focus_width) / 2.0).max(0.0);
    let focus_index = columns
        .iter()
        .position(|(column, _)| *column == focus_column)
        .unwrap_or(0);
    let mut xs = vec![0.0; columns.len()];
    xs[focus_index] = focus_x;
    for index in (focus_index + 1)..columns.len() {
        xs[index] = xs[index - 1] + columns[index - 1].1 + gap;
    }
    for index in (0..focus_index).rev() {
        xs[index] = xs[index + 1] - columns[index].1 - gap;
    }
    for (index, tile) in tiles.iter().enumerate() {
        if tile.exact.is_some() {
            continue;
        }
        let column_index = columns
            .iter()
            .position(|(column, _)| *column == tile.column)
            .unwrap_or(0);
        let mut y = top + gap;
        let mut rows: Vec<&NiriTile> = tiles
            .iter()
            .filter(|row| row.exact.is_none() && row.column == tile.column)
            .collect();
        rows.sort_by_key(|row| row.row);
        for row in rows.into_iter().take_while(|row| row.row < tile.row) {
            y += row.height + gap;
        }
        let x = xs[column_index] + (columns[column_index].1 - tile.width) / 2.0;
        placed[index] = (x, y, tile.width, tile.height);
    }
    // 全屏窗口覆盖整个输出，不参与居中估算。
    for (index, tile) in tiles.iter().enumerate() {
        if tile.exact.is_none() && tile.width >= view.0 - 1.0 && tile.height >= view.1 - 1.0 {
            placed[index] = (0.0, 0.0, view.0, view.1);
        }
    }
    placed
}

/// 只有合成器公布的可见顶层窗口几何可作为锚点，不假设 IPC 提供应用内部控件。
pub fn window_regions(output: &OutputInfo, physical: (u32, u32)) -> Vec<RectPx> {
    let sx = physical.0 as f64 / f64::from(output.width);
    let sy = physical.1 as f64 / f64::from(output.height);
    let mut rects = Vec::new();
    let mut add = |x: f64, y: f64, w: f64, h: f64| {
        let rect = RectPx::new(
            ((x - f64::from(output.x)) * sx).round() as i32,
            ((y - f64::from(output.y)) * sy).round() as i32,
            (w * sx).round().max(0.) as u32,
            (h * sy).round().max(0.) as u32,
        )
        .clamp_to(RectPx::new(0, 0, physical.0, physical.1));
        if rect.width > 2 && rect.height > 2 {
            rects.push(rect)
        }
    };
    match Backend::detect() {
        Ok(Backend::Hyprland) => {
            if let Ok(v) = hypr_query("clients")
                && let Some(list) = v.as_array()
            {
                // 焦点窗口排在最前，作为默认选择。
                let mut visible: Vec<&Value> = list
                    .iter()
                    .filter(|w| w["hidden"] != true && w["mapped"] != false)
                    .collect();
                visible.sort_by_key(|w| w["focused"] != true);
                for w in visible {
                    if let (Some(x), Some(y), Some(width), Some(height)) = (
                        w["at"][0].as_f64(),
                        w["at"][1].as_f64(),
                        w["size"][0].as_f64(),
                        w["size"][1].as_f64(),
                    ) {
                        add(x, y, width, height)
                    }
                }
            }
        }
        Ok(Backend::Niri) => {
            // 只取截图输出上当前活动工作区的窗口，避免其他显示器的坐标混入。
            let workspace = niri_query("Workspaces").ok().and_then(|v| {
                v["Workspaces"].as_array().and_then(|list| {
                    list.iter()
                        .find(|w| w["output"] == output.name.as_str() && w["is_active"] == true)
                        .and_then(|w| w["id"].as_u64())
                })
            });
            let mut tiles = Vec::new();
            if let Ok(v) = niri_query("Windows")
                && let Some(list) = v["Windows"].as_array()
            {
                for window in list {
                    if workspace.is_some() && window["workspace_id"].as_u64() != workspace {
                        continue;
                    }
                    let layout = &window["layout"];
                    let (Some(width), Some(height)) = (
                        layout["tile_size"][0].as_f64(),
                        layout["tile_size"][1].as_f64(),
                    ) else {
                        continue;
                    };
                    if width <= 1.0 || height <= 1.0 {
                        continue;
                    }
                    let index = |value: &Value, fallback: usize| {
                        value
                            .as_array()
                            .and_then(|pair| {
                                Some((
                                    pair.first()?.as_u64()? as usize,
                                    pair.get(1)?.as_u64()? as usize,
                                ))
                            })
                            .unwrap_or((fallback, fallback))
                    };
                    let (column, row) = index(&layout["pos_in_scrolling_layout"], 1);
                    let offset = (
                        layout["window_offset_in_tile"][0].as_f64().unwrap_or(0.0),
                        layout["window_offset_in_tile"][1].as_f64().unwrap_or(0.0),
                    );
                    let window_size = (
                        layout["window_size"][0].as_f64().unwrap_or(width),
                        layout["window_size"][1].as_f64().unwrap_or(height),
                    );
                    let exact = layout["tile_pos_in_workspace_view"][0]
                        .as_f64()
                        .zip(layout["tile_pos_in_workspace_view"][1].as_f64());
                    tiles.push(NiriTile {
                        column,
                        row,
                        width,
                        height,
                        offset,
                        window: window_size,
                        exact,
                        focused: window["is_focused"] == true,
                    });
                }
            }
            let placed = place_niri_tiles(
                &tiles,
                (f64::from(output.width), f64::from(output.height)),
                16.0,
            );
            let focused = tiles.iter().position(|tile| tile.focused);
            let order = focused
                .into_iter()
                .chain((0..tiles.len()).filter(|index| Some(*index) != focused));
            let origin = (f64::from(output.x), f64::from(output.y));
            for index in order {
                let (x, y, w, h) = placed[index];
                add(origin.0 + x, origin.1 + y, w, h);
                // 浮动/全屏窗口在 tile 内有实际内容偏移时，再提供内容区域。
                let (ox, oy) = tiles[index].offset;
                let (ww, wh) = tiles[index].window;
                let differs = ox.abs() > 2.0
                    || oy.abs() > 2.0
                    || (ww - w).abs() > 2.0
                    || (wh - h).abs() > 2.0;
                if differs && ox >= -1.0 && oy >= -1.0 && ox + ww <= w + 2.0 && oy + wh <= h + 2.0 {
                    add(origin.0 + x + ox, origin.1 + y + oy, ww, wh);
                }
            }
        }
        _ => {}
    }
    rects
}
use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(column: usize, width: f64, height: f64, focused: bool) -> NiriTile {
        NiriTile {
            column,
            row: 1,
            width,
            height,
            offset: (0.0, 0.0),
            window: (width, height),
            exact: None,
            focused,
        }
    }

    #[test]
    fn niri_reconstruction_centers_focused_column_and_keeps_gaps() {
        // 与 niri 26.04 的 2048×1152、scale 1.25、gaps 16、默认 2/3 列宽一致。
        let tiles = vec![
            tile(1, 1338.4, 1086.4, false),
            tile(2, 1338.4, 1086.4, true),
            tile(3, 1338.4, 1086.4, false),
        ];
        let placed = place_niri_tiles(&tiles, (2048.0, 1152.0), 16.0);
        let close = |a: f64, b: f64| (a - b).abs() < 0.6;
        assert!(close(placed[1].0, 354.8), "{:?}", placed);
        // 顶栏占 33.6 逻辑像素，上下各留 16。
        assert!(close(placed[1].1, 49.6), "{:?}", placed);
        assert!(close(placed[2].0, 354.8 + 1338.4 + 16.0), "{:?}", placed);
        assert!(close(placed[0].0, 354.8 - 1338.4 - 16.0), "{:?}", placed);
    }

    #[test]
    fn niri_exact_positions_are_used_when_all_are_known() {
        let mut exact = tile(1, 400.0, 300.0, true);
        exact.exact = Some((120.0, 80.0));
        let mut second = tile(2, 400.0, 300.0, false);
        second.exact = Some((536.0, 80.0));
        let placed = place_niri_tiles(&[exact, second], (1024.0, 768.0), 16.0);
        assert_eq!(placed[0], (120.0, 80.0, 400.0, 300.0));
        assert_eq!(placed[1], (536.0, 80.0, 400.0, 300.0));
    }

    #[test]
    fn niri_placement_handles_missing_window_list() {
        assert!(place_niri_tiles(&[], (2048.0, 1152.0), 16.0).is_empty());
    }

    #[test]
    fn niri_stacked_rows_are_placed_below_each_other() {
        let first = tile(1, 400.0, 300.0, true);
        let mut second = tile(1, 400.0, 300.0, false);
        second.row = 2;
        let placed = place_niri_tiles(&[first, second], (1024.0, 768.0), 16.0);
        assert!(
            close_enough(placed[1].1, placed[0].1 + 300.0 + 16.0),
            "{:?}",
            placed
        );
        assert!(close_enough(placed[0].0, placed[1].0));
    }

    fn close_enough(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.6
    }
}
