//! Controller input: a pointer ray for the library panel, and playback controls.
//!
//! Bindings are declared for several interaction profiles so the player works
//! whatever controllers are paired. The runtime picks whichever profile matches
//! the hardware; the simple controller is the universal fallback and is always
//! bound so that something works even on unfamiliar devices.

use anyhow::{Context, Result};
use openxr as xr;

/// Which hand an action came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hand {
    Left,
    /// Most people point with the right hand, so it owns the ray until the
    /// left one is actually used.
    #[default]
    Right,
}

impl Hand {
    pub const ALL: [Hand; 2] = [Hand::Left, Hand::Right];

    fn path_str(self) -> &'static str {
        match self {
            Hand::Left => "/user/hand/left",
            Hand::Right => "/user/hand/right",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Hand::Left => 0,
            Hand::Right => 1,
        }
    }
}

/// What the controllers are doing this frame.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    /// Aim pose per hand, when tracking is valid.
    pub aim: [Option<xr::Posef>; 2],
    /// Trigger pressed this frame, per hand — the "click" action.
    pub select: [bool; 2],
    /// Rising edge of select, which is what the UI actually acts on.
    pub select_pressed: [bool; 2],
    /// Rising edge of the button that shows or hides the playback controls.
    pub toggle_controls_pressed: bool,
    /// Recenter button rising edge: brings everything back in front of the
    /// viewer, wherever they have ended up sitting or facing.
    pub recenter_pressed: bool,
    /// Menu button rising edge: shows or hides the library.
    pub menu_pressed: bool,
    /// Thumbstick, x right-positive and y up-positive.
    pub stick: [(f32, f32); 2],
    /// Which hand most recently did something, so the pointer follows the hand
    /// actually being used rather than flickering between two.
    pub active_hand: Hand,
}

pub struct Input {
    action_set: xr::ActionSet,
    aim_action: xr::Action<xr::Posef>,
    select_action: xr::Action<bool>,
    menu_action: xr::Action<bool>,
    recenter_action: xr::Action<bool>,
    hide_action: xr::Action<bool>,
    stick_x: xr::Action<f32>,
    stick_y: xr::Action<f32>,
    aim_spaces: Vec<xr::Space>,
    hand_paths: Vec<xr::Path>,
    prev_select: [bool; 2],
    prev_menu: [bool; 2],
    prev_recenter: [bool; 2],
    prev_hide: [bool; 2],
    active_hand: Hand,
}

impl Input {
    pub fn new(instance: &xr::Instance, session: &xr::Session<xr::OpenGL>) -> Result<Self> {
        let action_set = instance
            .create_action_set("vrmp", "vrmp controls", 0)
            .context("creating action set")?;

        let hand_paths: Vec<xr::Path> = Hand::ALL
            .iter()
            .map(|h| instance.string_to_path(h.path_str()))
            .collect::<xr::Result<_>>()?;

        let aim_action =
            action_set.create_action::<xr::Posef>("aim", "Pointer", &hand_paths)?;
        let select_action = action_set.create_action::<bool>("select", "Select", &hand_paths)?;
        let menu_action = action_set.create_action::<bool>("menu", "Menu", &hand_paths)?;
        let hide_action =
            action_set.create_action::<bool>("hide", "Show or hide controls", &hand_paths)?;
        let recenter_action =
            action_set.create_action::<bool>("recenter", "Recenter view", &hand_paths)?;
        let stick_x = action_set.create_action::<f32>("stick_x", "Stick X", &hand_paths)?;
        let stick_y = action_set.create_action::<f32>("stick_y", "Stick Y", &hand_paths)?;

        // Each profile below lists the closest equivalent of the same actions.
        // `aim` uses the aim pose rather than grip, because the ray should leave
        // the controller pointing where it is aimed, not where it is held.
        bind_profile(
            instance,
            &action_set,
            "/interaction_profiles/khr/simple_controller",
            &aim_action,
            &select_action,
            &menu_action,
            &recenter_action,
            &hide_action,
            None,
            &["/input/select/click", "/input/select/click"],
            &["/input/menu/click", "/input/menu/click"],
            // No spare button on this profile.
            None,
            None,
        )?;

        bind_profile(
            instance,
            &action_set,
            "/interaction_profiles/valve/index_controller",
            &aim_action,
            &select_action,
            &menu_action,
            &recenter_action,
            &hide_action,
            Some((&stick_x, &stick_y)),
            &["/input/trigger/click", "/input/trigger/click"],
            &["/input/b/click", "/input/b/click"],
            Some(&["/input/thumbstick/click", "/input/thumbstick/click"]),
            Some(&["/input/a/click", "/input/a/click"]),
        )?;

        bind_profile(
            instance,
            &action_set,
            "/interaction_profiles/oculus/touch_controller",
            &aim_action,
            &select_action,
            &menu_action,
            &recenter_action,
            &hide_action,
            Some((&stick_x, &stick_y)),
            &["/input/trigger/value", "/input/trigger/value"],
            // Touch has menu/click on the left controller only; the right
            // hand uses B, its nearest equivalent. Binding menu/click on both
            // makes the runtime reject the entire profile.
            &["/input/menu/click", "/input/b/click"],
            Some(&["/input/thumbstick/click", "/input/thumbstick/click"]),
            // A exists only on the right Touch controller; the left has X in
            // the equivalent position.
            Some(&["/input/x/click", "/input/a/click"]),
        )?;

        bind_profile(
            instance,
            &action_set,
            "/interaction_profiles/htc/vive_controller",
            &aim_action,
            &select_action,
            &menu_action,
            &recenter_action,
            &hide_action,
            None,
            &["/input/trigger/click", "/input/trigger/click"],
            &["/input/menu/click", "/input/menu/click"],
            // No spare button on this profile.
            None,
            None,
        )?;

        session.attach_action_sets(&[&action_set])?;

        let aim_spaces = Hand::ALL
            .iter()
            .map(|h| {
                aim_action.create_space(session, hand_paths[h.index()], xr::Posef::IDENTITY)
            })
            .collect::<xr::Result<_>>()?;

        Ok(Input {
            action_set,
            aim_action,
            select_action,
            menu_action,
            recenter_action,
            hide_action,
            stick_x,
            stick_y,
            aim_spaces,
            hand_paths,
            prev_select: [false; 2],
            prev_menu: [false; 2],
            prev_recenter: [false; 2],
            prev_hide: [false; 2],
            active_hand: Hand::Right,
        })
    }

    /// Samples all actions for this frame.
    pub fn sync(
        &mut self,
        session: &xr::Session<xr::OpenGL>,
        base: &xr::Space,
        time: xr::Time,
    ) -> Result<InputState> {
        session.sync_actions(&[(&self.action_set).into()])?;

        let mut state = InputState { active_hand: self.active_hand, ..Default::default() };

        for hand in Hand::ALL {
            let i = hand.index();
            let subaction = self.hand_paths[i];

            let location = self.aim_spaces[i].locate(base, time)?;
            if location
                .location_flags
                .contains(xr::SpaceLocationFlags::POSITION_VALID)
            {
                state.aim[i] = Some(location.pose);
            }

            let select = self
                .select_action
                .state(session, subaction)?
                .current_state;
            state.select[i] = select;
            state.select_pressed[i] = select && !self.prev_select[i];
            self.prev_select[i] = select;

            let menu = self.menu_action.state(session, subaction)?.current_state;
            if menu && !self.prev_menu[i] {
                state.menu_pressed = true;
            }
            self.prev_menu[i] = menu;

            let recenter = self.recenter_action.state(session, subaction)?.current_state;
            if recenter && !self.prev_recenter[i] {
                state.recenter_pressed = true;
            }
            self.prev_recenter[i] = recenter;

            let hide = self.hide_action.state(session, subaction)?.current_state;
            if hide && !self.prev_hide[i] {
                state.toggle_controls_pressed = true;
            }
            self.prev_hide[i] = hide;

            let x = self.stick_x.state(session, subaction)?.current_state;
            let y = self.stick_y.state(session, subaction)?.current_state;
            state.stick[i] = (x, y);

            // Any deliberate action makes this the active hand.
            if select || x.abs() > 0.3 || y.abs() > 0.3 {
                self.active_hand = hand;
                state.active_hand = hand;
            }
        }

        Ok(state)
    }
}

/// Suggests bindings for one interaction profile.
///
/// Profiles that lack a thumbstick simply omit those bindings; the runtime
/// rejects the whole suggestion if any path is invalid for the profile, so each
/// is declared separately rather than as one merged list.
#[allow(clippy::too_many_arguments)]
fn bind_profile(
    instance: &xr::Instance,
    action_set: &xr::ActionSet,
    profile: &str,
    aim: &xr::Action<xr::Posef>,
    select: &xr::Action<bool>,
    menu: &xr::Action<bool>,
    recenter: &xr::Action<bool>,
    hide: &xr::Action<bool>,
    stick: Option<(&xr::Action<f32>, &xr::Action<f32>)>,
    select_paths: &[&str; 2],
    menu_paths: &[&str; 2],
    recenter_paths: Option<&[&str; 2]>,
    hide_paths: Option<&[&str; 2]>,
) -> Result<()> {
    let _ = action_set;
    let profile_path = instance.string_to_path(profile)?;

    // Resolve every path first and keep them alive, since the bindings borrow
    // them. Collected as (action, path) pairs rather than indexed into a flat
    // list: the previous arithmetic version silently bound the wrong paths
    // whenever a profile's action set changed shape.
    let mut paths: Vec<(&str, xr::Path)> = Vec::new();
    for (i, hand) in Hand::ALL.iter().enumerate() {
        let base = hand.path_str();
        let mut add = |role: &'static str, suffix: &str| -> Result<()> {
            paths.push((role, instance.string_to_path(&format!("{base}{suffix}"))?));
            Ok(())
        };
        add("aim", "/input/aim/pose")?;
        add("select", select_paths[i])?;
        add("menu", menu_paths[i])?;
        if stick.is_some() {
            add("stick_x", "/input/thumbstick/x")?;
            add("stick_y", "/input/thumbstick/y")?;
        }
        if let Some(recenter_paths) = recenter_paths {
            add("recenter", recenter_paths[i])?;
        }
        if let Some(hide_paths) = hide_paths {
            add("hide", hide_paths[i])?;
        }
    }

    let mut bindings: Vec<xr::Binding> = Vec::new();
    for (role, path) in &paths {
        let binding = match *role {
            "aim" => xr::Binding::new(aim, *path),
            "select" => xr::Binding::new(select, *path),
            "menu" => xr::Binding::new(menu, *path),
            "recenter" => xr::Binding::new(recenter, *path),
            "hide" => xr::Binding::new(hide, *path),
            "stick_x" => match stick {
                Some((sx, _)) => xr::Binding::new(sx, *path),
                None => continue,
            },
            "stick_y" => match stick {
                Some((_, sy)) => xr::Binding::new(sy, *path),
                None => continue,
            },
            _ => continue,
        };
        bindings.push(binding);
    }

    // A profile the runtime does not know is not an error worth aborting for:
    // another profile will match the hardware actually in use.
    if let Err(e) = instance.suggest_interaction_profile_bindings(profile_path, &bindings) {
        eprintln!("note: runtime rejected bindings for {profile}: {e}");
    }
    Ok(())
}
