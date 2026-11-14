use std::{collections::BTreeMap, ops::Range, time::Duration};

use clock::Global;
use collections::{HashMap, HashSet};
use futures::future::join_all;
use gpui::{ClickEvent, Context, Entity, Focusable as _, Task, Window};
use language::{Buffer, BufferRow, SymbolKind};
use multi_buffer::{MultiBufferRow, ToPoint as _};
use project::{DocumentSymbol, LocationLink, Project};
use settings::Settings as _;
use text::{Bias, BufferId, PointUtf16, ToPoint as _, Unclipped};
use ui::{Color, IconButton, IconName, IconSize, Tooltip, prelude::*};

use crate::{
    Editor, EditorSettings, GoToImplementation, SelectionEffects,
    display_map::{DisplayRow, ToDisplayPoint as _},
};

/// Implementation lookups are one LSP request per symbol, so they are debounced
/// harder than other LSP data to avoid flooding the server while typing.
const IMPLEMENTATION_MARKERS_DEBOUNCE: Duration = Duration::from_millis(800);

/// Bounds the number of `textDocument/implementation` requests issued per refresh
/// so that huge generated files do not stall the language server.
const MAX_SYMBOLS_PER_BUFFER: usize = 300;

#[derive(Clone, Debug)]
pub(crate) struct ImplementationMarker {
    pub position: text::Anchor,
    pub implementation_count: usize,
}

pub(crate) struct ImplementationMarkers {
    markers: HashMap<BufferId, (Global, BTreeMap<BufferRow, ImplementationMarker>)>,
    update_task: Task<()>,
}

impl ImplementationMarkers {
    pub fn new() -> Self {
        Self {
            markers: HashMap::default(),
            update_task: Task::ready(()),
        }
    }

    fn has_cached(&self, buffer_id: BufferId, version: &Global) -> bool {
        self.markers
            .get(&buffer_id)
            .is_some_and(|(cached_version, _)| !version.changed_since(cached_version))
    }

    pub fn clear(&mut self, buffer_id: Option<BufferId>) {
        match buffer_id {
            Some(buffer_id) => {
                self.markers.remove(&buffer_id);
            }
            None => self.markers.clear(),
        }
    }
}

fn may_have_implementations(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Interface
            | SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Method
            | SymbolKind::Function
    )
}

fn collect_candidate_positions(symbols: &[DocumentSymbol], positions: &mut Vec<PointUtf16>) {
    for symbol in symbols {
        if positions.len() >= MAX_SYMBOLS_PER_BUFFER {
            return;
        }
        if may_have_implementations(symbol.kind) {
            positions.push(symbol.selection_range.start.0);
        }
        collect_candidate_positions(&symbol.children, positions);
    }
}

impl Editor {
    pub(crate) fn refresh_implementation_markers(
        &mut self,
        invalidate_buffer_data: Option<BufferId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode().is_full()
            || !EditorSettings::get_global(cx).gutter.implementations
            || !self.lsp_data_enabled()
        {
            self.implementation_markers.clear(None);
            return;
        }
        let Some(buffer) = self.buffer().read(cx).as_singleton() else {
            self.implementation_markers.clear(None);
            return;
        };
        let Some(project) = self.project().cloned() else {
            return;
        };
        let buffer_read = buffer.read(cx);
        if buffer_read.file().is_none() {
            self.implementation_markers.clear(None);
            return;
        }
        let buffer_id = buffer_read.remote_id();
        if invalidate_buffer_data != Some(buffer_id)
            && self
                .implementation_markers
                .has_cached(buffer_id, &buffer_read.version())
        {
            return;
        }

        self.implementation_markers.update_task = cx.spawn_in(window, async move |editor, cx| {
            cx.background_executor()
                .timer(IMPLEMENTATION_MARKERS_DEBOUNCE)
                .await;
            let markers = fetch_implementation_markers(project, buffer.clone(), cx).await;
            editor
                .update(cx, |editor, cx| {
                    let version = buffer.read(cx).version();
                    editor
                        .implementation_markers
                        .markers
                        .insert(buffer_id, (version, markers));
                    cx.notify();
                })
                .ok();
        });
    }

    pub(crate) fn active_implementation_markers(
        &self,
        range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<DisplayRow, ImplementationMarker> {
        let mut result = HashMap::default();
        if self.implementation_markers.markers.is_empty() {
            return result;
        }
        let snapshot = self.snapshot(window, cx);
        let multi_buffer_snapshot = snapshot.buffer_snapshot();
        let mut seen_rows = HashSet::default();
        for (_, markers) in self.implementation_markers.markers.values() {
            for marker in markers.values() {
                let Some(anchor) = multi_buffer_snapshot.anchor_in_excerpt(marker.position) else {
                    continue;
                };
                let multi_buffer_point = anchor.to_point(multi_buffer_snapshot);
                if snapshot.is_line_folded(MultiBufferRow(multi_buffer_point.row))
                    && multi_buffer_point
                        .row
                        .checked_sub(1)
                        .is_some_and(|previous_row| {
                            snapshot.is_line_folded(MultiBufferRow(previous_row))
                        })
                {
                    continue;
                }
                let display_row = multi_buffer_point.to_display_point(&snapshot).row();
                if range.contains(&display_row) && seen_rows.insert(display_row) {
                    result.insert(display_row, marker.clone());
                }
            }
        }
        result
    }

    pub(crate) fn render_implementation_marker(
        &self,
        marker: &ImplementationMarker,
        row: DisplayRow,
        cx: &mut Context<Self>,
    ) -> IconButton {
        let position = marker.position;
        let tooltip = if marker.implementation_count == 1 {
            "Go to Implementation".to_string()
        } else {
            format!("Go to Implementation ({})", marker.implementation_count)
        };
        IconButton::new(
            ("implementation_marker", row.0 as usize),
            IconName::GoToImplementation,
        )
        .shape(ui::IconButtonShape::Square)
        .icon_size(IconSize::XSmall)
        .icon_color(Color::Accent)
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(move |editor, _: &ClickEvent, window, cx| {
            editor.go_to_implementation_at(position, window, cx);
        }))
    }

    fn go_to_implementation_at(
        &mut self,
        position: text::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_buffer_snapshot = self.buffer().read(cx).snapshot(cx);
        let Some(anchor) = multi_buffer_snapshot.anchor_in_excerpt(position) else {
            return;
        };
        window.focus(&self.focus_handle(cx), cx);
        self.change_selections(SelectionEffects::default(), window, cx, |selections| {
            selections.select_anchor_ranges([anchor..anchor]);
        });
        self.go_to_implementation(&GoToImplementation::default(), window, cx)
            .detach_and_log_err(cx);
    }
}

async fn fetch_implementation_markers(
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    cx: &mut gpui::AsyncWindowContext,
) -> BTreeMap<BufferRow, ImplementationMarker> {
    let mut markers = BTreeMap::new();
    let symbols_task = project.update(cx, |project, cx| project.document_symbols(&buffer, cx));
    let symbols = match symbols_task.await {
        Ok(symbols) => symbols,
        Err(error) => {
            log::debug!("failed to fetch document symbols for implementation markers: {error:#}");
            return markers;
        }
    };

    let mut positions = Vec::new();
    collect_candidate_positions(&symbols, &mut positions);
    if positions.is_empty() {
        return markers;
    }

    let requests = project.update(cx, |project, cx| {
        positions
            .iter()
            .map(|position| project.implementations(&buffer, *position, cx))
            .collect::<Vec<_>>()
    });
    let responses = join_all(requests).await;

    buffer.read_with(cx, |buffer_read, _| {
        let snapshot = buffer_read.snapshot();
        for (position, response) in positions.into_iter().zip(responses) {
            let links: Vec<LocationLink> = match response {
                Ok(Some(links)) => links,
                Ok(None) => continue,
                Err(error) => {
                    log::debug!("implementation request failed: {error:#}");
                    continue;
                }
            };
            let point = snapshot.clip_point_utf16(Unclipped(position), Bias::Left);
            let symbol_row = point.row;
            let implementation_count = links
                .iter()
                .filter(|link| {
                    // Servers commonly include the declaration itself in the
                    // response; that is not an implementation worth a marker.
                    link.target.buffer != buffer
                        || link.target.range.start.to_point(&snapshot).row != symbol_row
                })
                .count();
            if implementation_count == 0 {
                continue;
            }
            markers.entry(symbol_row).or_insert(ImplementationMarker {
                position: snapshot.anchor_before(point),
                implementation_count,
            });
        }
    });
    markers
}
