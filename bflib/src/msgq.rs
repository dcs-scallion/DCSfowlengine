/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

use dcso3::{
    Color, LuaVec3, String, Vector2, Vector3,
    coalition::Side,
    env::miz::{GroupId, UnitId},
    net::{Net, PlayerId},
    trigger::{
        Action, ArrowSpec, CircleSpec, LineSpec, LineType, MarkId, QuadSpec, RectSpec,
        SideFilter, TextSpec,
    },
};
use log::error;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub enum PanelDest {
    All,
    Side(Side),
    Group(GroupId),
    Unit(UnitId),
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum MarkDest {
    All,
    Side(Side),
    Group(GroupId),
}

#[derive(Debug, Clone)]
pub enum MsgTyp {
    Chat(Option<PlayerId>),
    Panel {
        to: PanelDest,
        display_time: i64,
        clear_view: bool,
    },
    Mark {
        id: MarkId,
        to: MarkDest,
        position: LuaVec3,
        read_only: bool,
        message: Option<String>,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Msg {
    Message {
        typ: MsgTyp,
        text: String,
    },
    Circle {
        id: MarkId,
        to: SideFilter,
        spec: CircleSpec,
        message: Option<String>,
    },
    Rect {
        id: MarkId,
        to: SideFilter,
        spec: RectSpec,
        message: Option<String>,
    },
    Quad {
        id: MarkId,
        to: SideFilter,
        spec: QuadSpec,
        message: Option<String>,
    },
    Text {
        id: MarkId,
        to: SideFilter,
        spec: TextSpec,
    },
    Arrow {
        id: MarkId,
        to: SideFilter,
        spec: ArrowSpec,
        message: Option<String>,
    },
    Line {
        id: MarkId,
        to: SideFilter,
        spec: LineSpec,
        message: Option<String>,
    },
    Freeform {
        id: MarkId,
        to: SideFilter,
        points: [LuaVec3; 3],
        color: Color,
        fill_color: Color,
        line_type: LineType,
        read_only: bool,
        message: Option<String>,
    },
    SetMarkupColor {
        id: MarkId,
        color: Color,
    },
    SetMarkupFillColor {
        id: MarkId,
        color: Color,
    },
    SetMarkupText {
        id: MarkId,
        text: String,
    },
    SetMarkupStart {
        id: MarkId,
        pos: LuaVec3,
    },
    SetMarkupEnd {
        id: MarkId,
        pos: LuaVec3,
    },
}

#[derive(Debug, Clone)]
pub enum Cmd {
    Send(Msg),
    DeleteMark(MarkId),
}

#[derive(Debug, Clone)]
pub struct MsgQ(Vec<VecDeque<Cmd>>);

/// DCS F10 draw order: lower index is sent first (under later layers).
const PRI_PANEL: usize = 0;
/// Supply arrows, production feed, front line, occupied-hub lines.
const PRI_LINE: usize = 1;
/// Objective zone rings, threat/capturable circles.
const PRI_SHAPE: usize = 2;
const PRI_TEXT: usize = 3;
/// Objective name + infobar stats (top layer; re-sent after underlay updates).
const PRI_OVERLAY: usize = 4;
const PRI_COUNT: usize = 5;

impl Default for MsgQ {
    fn default() -> Self {
        MsgQ((0..PRI_COUNT).map(|_| VecDeque::default()).collect())
    }
}

impl MsgQ {
    fn send_with_priority<S: Into<String>>(&mut self, p: usize, typ: MsgTyp, text: S) {
        self.0[p].push_back(Cmd::Send(Msg::Message {
            typ,
            text: text.into(),
        }))
    }

    pub fn send<S: Into<String>>(&mut self, typ: MsgTyp, text: S) {
        self.send_with_priority(PRI_PANEL, typ, text)
    }

    fn cancel_pending_mark_cmds(&mut self, did: MarkId) -> bool {
        let mut push = true;
        let mut remove = |pri: usize| {
            self.0[pri].retain(|cmd| match cmd {
                Cmd::DeleteMark(_) => true,
                Cmd::Send(msg) => match msg {
                    Msg::Message { .. } => true,
                    Msg::Circle { id, .. }
                    | Msg::Rect { id, .. }
                    | Msg::Quad { id, .. }
                    | Msg::Text { id, .. }
                    | Msg::Arrow { id, .. }
                    | Msg::Line { id, .. }
                    | Msg::Freeform { id, .. } => {
                        if *id == did {
                            push = false;
                            false
                        } else {
                            true
                        }
                    }
                    Msg::SetMarkupColor { id, .. }
                    | Msg::SetMarkupFillColor { id, .. }
                    | Msg::SetMarkupText { id, .. }
                    | Msg::SetMarkupStart { id, .. }
                    | Msg::SetMarkupEnd { id, .. } => *id != did,
                },
            })
        };
        for pri in 0..PRI_COUNT {
            remove(pri);
        }
        push
    }

    pub fn delete_mark(&mut self, did: MarkId) {
        if self.cancel_pending_mark_cmds(did) {
            self.0[PRI_SHAPE].push_back(Cmd::DeleteMark(did));
        }
    }

    /// Underlay shapes use PRI_LINE; delete must run on that queue before re-send.
    pub fn delete_underlay_mark(&mut self, did: MarkId) {
        if self.cancel_pending_mark_cmds(did) {
            self.0[PRI_LINE].push_front(Cmd::DeleteMark(did));
        }
    }

    #[allow(dead_code)]
    pub fn mark_to_all<S: Into<String>>(
        &mut self,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            PRI_SHAPE,
            MsgTyp::Mark {
                id,
                to: MarkDest::All,
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
                message: None,
            },
            text,
        );
        id
    }

    pub fn mark_to_side<S: Into<String>>(
        &mut self,
        side: Side,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            PRI_SHAPE,
            MsgTyp::Mark {
                id,
                to: MarkDest::Side(side),
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
                message: None,
            },
            text,
        );
        id
    }

    pub fn coalition_point_mark(
        &mut self,
        side: Side,
        id: MarkId,
        position: LuaVec3,
        read_only: bool,
        label: String,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Message {
            typ: MsgTyp::Mark {
                id,
                to: MarkDest::Side(side),
                position,
                read_only,
                message,
            },
            text: label,
        }));
    }

    pub fn circle_to_side(
        &mut self,
        side: Side,
        id: MarkId,
        spec: CircleSpec,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Circle {
            id,
            to: side.into(),
            spec,
            message,
        }));
    }

    #[allow(dead_code)]
    pub fn mark_to_group<S: Into<String>>(
        &mut self,
        group: GroupId,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            PRI_SHAPE,
            MsgTyp::Mark {
                id,
                to: MarkDest::Group(group),
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
                message: None,
            },
            text,
        );
        id
    }

    #[allow(dead_code)]
    pub fn panel_to_all<S: Into<String>>(&mut self, display_time: i64, clear_view: bool, text: S) {
        self.send_with_priority(
            PRI_PANEL,
            MsgTyp::Panel {
                to: PanelDest::All,
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_side<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        side: Side,
        text: S,
    ) {
        self.send_with_priority(
            PRI_PANEL,
            MsgTyp::Panel {
                to: PanelDest::Side(side),
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_group<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        group: GroupId,
        text: S,
    ) {
        self.send_with_priority(
            PRI_PANEL,
            MsgTyp::Panel {
                to: PanelDest::Group(group),
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_unit<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        unit: UnitId,
        text: S,
    ) {
        self.send_with_priority(
            PRI_PANEL,
            MsgTyp::Panel {
                to: PanelDest::Unit(unit),
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn circle_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: CircleSpec,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Circle {
            id,
            to,
            spec,
            message,
        }))
    }

    #[allow(dead_code)]
    pub fn rect_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: RectSpec,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Rect {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn quad_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: QuadSpec,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Quad {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn quad_to_underlay(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: QuadSpec,
        message: Option<String>,
    ) {
        self.0[PRI_LINE].push_back(Cmd::Send(Msg::Quad {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn text_to_all(&mut self, to: SideFilter, id: MarkId, spec: TextSpec) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::Text { id, to, spec }))
    }

    pub fn text_to_overlay(&mut self, to: SideFilter, id: MarkId, spec: TextSpec) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::Text { id, to, spec }))
    }

    pub fn arrow_to(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: ArrowSpec,
        message: Option<String>,
    ) {
        self.0[PRI_LINE].push_back(Cmd::Send(Msg::Arrow {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn line_to(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: LineSpec,
        message: Option<String>,
    ) {
        self.0[PRI_LINE].push_back(Cmd::Send(Msg::Line {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn freeform_to(
        &mut self,
        to: SideFilter,
        id: MarkId,
        points: [LuaVec3; 3],
        color: Color,
        fill_color: Color,
        line_type: LineType,
        read_only: bool,
        message: Option<String>,
    ) {
        self.0[PRI_SHAPE].push_back(Cmd::Send(Msg::Freeform {
            id,
            to,
            points,
            color,
            fill_color,
            line_type,
            read_only,
            message,
        }))
    }

    pub fn freeform_to_underlay(
        &mut self,
        to: SideFilter,
        id: MarkId,
        points: [LuaVec3; 3],
        color: Color,
        fill_color: Color,
        line_type: LineType,
        read_only: bool,
        message: Option<String>,
    ) {
        self.0[PRI_LINE].push_back(Cmd::Send(Msg::Freeform {
            id,
            to,
            points,
            color,
            fill_color,
            line_type,
            read_only,
            message,
        }))
    }

    pub fn set_markup_color(&mut self, id: MarkId, color: Color) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::SetMarkupColor { id, color }))
    }

    pub fn set_overlay_markup_color(&mut self, id: MarkId, color: Color) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::SetMarkupColor { id, color }))
    }

    #[allow(dead_code)]
    pub fn set_markup_fill_color(&mut self, id: MarkId, color: Color) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::SetMarkupFillColor { id, color }))
    }

    pub fn set_overlay_markup_fill_color(&mut self, id: MarkId, color: Color) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::SetMarkupFillColor { id, color }))
    }

    pub fn set_markup_text(&mut self, id: MarkId, text: String) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::SetMarkupText { id, text }))
    }

    pub fn set_overlay_markup_text(&mut self, id: MarkId, text: String) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::SetMarkupText { id, text }))
    }

    pub fn set_markup_pos_start(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::SetMarkupStart { id, pos }))
    }

    pub fn set_overlay_markup_pos_start(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::SetMarkupStart { id, pos }))
    }

    pub fn set_markup_pos_end(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[PRI_TEXT].push_back(Cmd::Send(Msg::SetMarkupEnd { id, pos }))
    }

    pub fn set_overlay_markup_pos_end(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[PRI_OVERLAY].push_back(Cmd::Send(Msg::SetMarkupEnd { id, pos }))
    }

    pub fn len(&self) -> usize {
        self.0.iter().fold(0, |acc, q| acc + q.len())
    }

    pub fn process(&mut self, max_rate: usize, net: &Net, act: &Action) {
        for _ in 0..max_rate {
            let cmd = match (0..PRI_COUNT).find_map(|pri| self.0[pri].pop_front()) {
                Some(cmd) => cmd,
                None => return,
            };
            let res = match cmd {
                Cmd::DeleteMark(id) => act.remove_mark(id),
                Cmd::Send(Msg::Message { typ, text }) => match typ {
                    MsgTyp::Mark {
                        id,
                        to,
                        position,
                        read_only,
                        message,
                    } => match to {
                        MarkDest::All => {
                            act.mark_to_all(id, text, position, read_only, message)
                        }
                        MarkDest::Side(side) => act.mark_to_coalition(
                            id,
                            text,
                            position,
                            side,
                            read_only,
                            message,
                        ),
                        MarkDest::Group(group) => {
                            act.mark_to_group(id, text, position, group, read_only, message)
                        }
                    },
                    MsgTyp::Chat(to) => match to {
                        None => net.send_chat(text, true),
                        Some(id) => net.send_chat_to(text, id, Some(PlayerId::from(1))),
                    },
                    MsgTyp::Panel {
                        to,
                        display_time,
                        clear_view,
                    } => match to {
                        PanelDest::All => act.out_text(text, display_time, clear_view),
                        PanelDest::Group(gid) => {
                            act.out_text_for_group(gid, text, display_time, clear_view)
                        }
                        PanelDest::Side(side) => {
                            act.out_text_for_coalition(side, text, display_time, clear_view)
                        }
                        PanelDest::Unit(uid) => {
                            act.out_text_for_unit(uid, text, display_time, clear_view)
                        }
                    },
                },
                Cmd::Send(Msg::Circle {
                    id,
                    to,
                    spec,
                    message,
                }) => act.circle_to_all(to, id, spec, message),
                Cmd::Send(Msg::Rect {
                    id,
                    to,
                    spec,
                    message,
                }) => act.rect_to_all(to, id, spec, message),
                Cmd::Send(Msg::Quad {
                    id,
                    to,
                    spec,
                    message,
                }) => act.quad_to_all(to, id, spec, message),
                Cmd::Send(Msg::Text { id, to, spec }) => act.text_to_all(to, id, spec),
                Cmd::Send(Msg::Arrow {
                    id,
                    to,
                    spec,
                    message,
                }) => act.arrow_to_all(to, id, spec, message),
                Cmd::Send(Msg::Line {
                    id,
                    to,
                    spec,
                    message,
                }) => act.line_to_all(to, id, spec, message),
                Cmd::Send(Msg::Freeform {
                    id,
                    to,
                    points,
                    color,
                    fill_color,
                    line_type,
                    read_only,
                    message,
                }) => act.freeform_to_all(
                    to,
                    id,
                    points,
                    color,
                    fill_color,
                    line_type,
                    read_only,
                    message,
                ),
                Cmd::Send(Msg::SetMarkupColor { id, color }) => act.set_markup_color(id, color),
                Cmd::Send(Msg::SetMarkupFillColor { id, color }) => {
                    act.set_markup_fill_color(id, color)
                }
                Cmd::Send(Msg::SetMarkupStart { id, pos }) => {
                    act.set_markup_position_start(id, pos)
                }
                Cmd::Send(Msg::SetMarkupEnd { id, pos }) => act.set_markup_position_end(id, pos),
                Cmd::Send(Msg::SetMarkupText { id, text }) => act.set_markup_text(id, text),
            };
            if let Err(e) = res {
                error!("could not send message {:?}", e)
            }
        }
    }
}
