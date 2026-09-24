//! Turns a controller aim pose into a position on the UI panel.
//!
//! The interface is an OpenXR cylinder layer wrapped around the viewer, so
//! pointing at it is a ray-cylinder intersection. The hit point becomes a 2D
//! coordinate that egui consumes as an ordinary mouse position, which is what
//! lets the whole UI be written as normal 2D code.

use glam::{Quat, Vec3};
use openxr as xr;

/// Placement and size of the curved UI panel.
#[derive(Debug, Clone, Copy)]
pub struct CylinderPanel {
    /// Where the cylinder's axis stands and which way its centre faces.
    pub pose: xr::Posef,
    /// Distance from the axis to the panel surface, in metres.
    pub radius: f32,
    /// Horizontal arc the panel spans, in radians.
    pub central_angle: f32,
    /// Width divided by height of the panel image.
    pub aspect_ratio: f32,
}

impl CylinderPanel {
    /// A panel centred on `yaw`, at `eye_height`, `distance` metres away,
    /// spanning `angle_deg` of horizontal arc.
    ///
    /// The arc is what controls apparent size. Panel height follows from it —
    /// arc length divided by aspect ratio — so widening the angle grows the
    /// panel in both directions at once. Much past 60° it stops reading as a
    /// window in front of you and starts wrapping around your head.
    pub fn facing(
        yaw: f32,
        head: xr::Vector3f,
        distance: f32,
        aspect_ratio: f32,
        angle_deg: f32,
    ) -> Self {
        Self::facing_with_pitch(yaw, head, distance, aspect_ratio, angle_deg, 0.0, 0.0)
    }

    /// As [`Self::facing`], but reclined with the viewer and dropped
    /// `below_deg` beneath their eye line.
    ///
    /// `recline` is how far back the viewer is lying, so content follows them
    /// onto their back rather than staying bolted to the room's horizon.
    /// `below_deg` is the separate, fixed offset that keeps the transport bar
    /// under the content instead of over it — the two are added, so the bar sits
    /// below the viewer's gaze whatever angle they are at.
    pub fn facing_with_pitch(
        yaw: f32,
        head: xr::Vector3f,
        distance: f32,
        aspect_ratio: f32,
        angle_deg: f32,
        recline: f32,
        below_deg: f32,
    ) -> Self {
        // Expressed as rotation rather than a drop in height, so it still points
        // at the viewer when they are lying down. Lowering by metres only works
        // while "down" and "away from the eye line" are the same direction.
        let pitch = recline - below_deg.to_radians();
        CylinderPanel {
            // Centred on the viewer's head, not the room origin. The panel is a
            // cylinder of `distance` radius about this point, so anchoring it
            // anywhere else puts the viewer off-centre inside it — nearer on one
            // side than the other, and a short strip can miss their view.
            pose: crate::xr::pose_facing(yaw, pitch, head),
            radius: distance,
            central_angle: angle_deg.clamp(15.0, 160.0).to_radians(),
            aspect_ratio,
        }
    }

    /// Height of the panel in metres, implied by its arc length and aspect.
    pub fn height(&self) -> f32 {
        (self.radius * self.central_angle) / self.aspect_ratio
    }

    /// Where an aim ray meets the panel, in pixels, or `None` if it misses.
    ///
    /// `region` is the part of the egui surface this panel displays. The
    /// playback controls occupy a short wide strip of the shared surface rather
    /// than the whole of it, so the hit has to be mapped back into that strip's
    /// pixel coordinates for egui to receive a sensible pointer position.
    pub fn hit_in(&self, aim: xr::Posef, region: egui::Rect) -> Option<egui::Pos2> {
        let (u, v) = self.hit_uv(aim)?;
        Some(egui::pos2(
            region.min.x + u * region.width(),
            region.min.y + v * region.height(),
        ))
    }

    /// Where an aim ray meets the panel, in pixels of a full-surface panel.
    pub fn hit(&self, aim: xr::Posef, surface: (f32, f32)) -> Option<egui::Pos2> {
        let (u, v) = self.hit_uv(aim)?;
        Some(egui::pos2(u * surface.0, v * surface.1))
    }

    /// Normalised coordinates across the panel, or `None` if the ray misses it.
    fn hit_uv(&self, aim: xr::Posef) -> Option<(f32, f32)> {
        let (origin, direction) = ray_from_pose(aim);

        // Work in the cylinder's own frame, where its axis is Y and its centre
        // faces -Z.
        let inv_rot = quat(self.pose.orientation).inverse();
        let local_origin = inv_rot * (origin - vec3(self.pose.position));
        let local_dir = inv_rot * direction;

        // Intersect with the infinite cylinder x^2 + z^2 = r^2.
        let a = local_dir.x * local_dir.x + local_dir.z * local_dir.z;
        if a < 1e-6 {
            // Ray is parallel to the axis and can never meet the surface.
            return None;
        }
        let b = 2.0 * (local_origin.x * local_dir.x + local_origin.z * local_dir.z);
        let c = local_origin.x * local_origin.x + local_origin.z * local_origin.z
            - self.radius * self.radius;
        let discriminant = b * b - 4.0 * a * c;
        if discriminant < 0.0 {
            return None;
        }

        // The viewer stands inside the cylinder, so the forward intersection is
        // the larger root.
        let sqrt_d = discriminant.sqrt();
        let t = (-b + sqrt_d) / (2.0 * a);
        if t <= 0.0 {
            return None;
        }

        let hit = local_origin + local_dir * t;

        // Horizontal position: angle away from the panel's centre direction.
        let angle = hit.x.atan2(-hit.z);
        if angle.abs() > self.central_angle * 0.5 {
            return None;
        }

        let height = self.height();
        if hit.y.abs() > height * 0.5 {
            return None;
        }

        let u = 0.5 + angle / self.central_angle;
        // Panel space runs upwards; pixel space runs downwards.
        let v = 0.5 - hit.y / height;

        Some((u, v))
    }
}

/// Origin and forward direction of an OpenXR pose. Aim poses point along -Z.
fn ray_from_pose(pose: xr::Posef) -> (Vec3, Vec3) {
    let rotation = quat(pose.orientation);
    (vec3(pose.position), rotation * Vec3::NEG_Z)
}

fn quat(q: xr::Quaternionf) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

/// The room origin, used as a neutral head position in tests.
#[cfg(test)]
const ORIGIN: xr::Vector3f = xr::Vector3f { x: 0.0, y: 0.0, z: 0.0 };

fn vec3(v: xr::Vector3f) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_panel() -> CylinderPanel {
        CylinderPanel::facing(0.0, ORIGIN, 2.0, 16.0 / 9.0, 100.0)
    }

    fn pose_looking(dir: Vec3) -> xr::Posef {
        let rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize());
        xr::Posef {
            orientation: xr::Quaternionf {
                x: rotation.x,
                y: rotation.y,
                z: rotation.z,
                w: rotation.w,
            },
            position: xr::Vector3f { x: 0.0, y: 0.0, z: 0.0 },
        }
    }

    #[test]
    fn straight_ahead_hits_the_centre() {
        let panel = identity_panel();
        let hit = panel
            .hit(pose_looking(Vec3::NEG_Z), (1000.0, 500.0))
            .expect("forward ray should hit the panel");
        assert!((hit.x - 500.0).abs() < 1.0, "x was {}", hit.x);
        assert!((hit.y - 250.0).abs() < 1.0, "y was {}", hit.y);
    }

    #[test]
    fn looking_right_moves_the_hit_right() {
        let panel = identity_panel();
        let dir = Quat::from_rotation_y(-20f32.to_radians()) * Vec3::NEG_Z;
        let hit = panel.hit(pose_looking(dir), (1000.0, 500.0)).unwrap();
        assert!(hit.x > 500.0, "expected right of centre, got {}", hit.x);
    }

    #[test]
    fn looking_away_misses() {
        let panel = identity_panel();
        assert!(panel.hit(pose_looking(Vec3::Z), (1000.0, 500.0)).is_none());
    }

    #[test]
    fn looking_far_up_misses() {
        let panel = identity_panel();
        let dir = Quat::from_rotation_x(70f32.to_radians()) * Vec3::NEG_Z;
        assert!(panel.hit(pose_looking(dir), (1000.0, 500.0)).is_none());
    }
}

#[cfg(test)]
mod region_tests {
    use super::*;

    fn pose_looking(dir: Vec3) -> xr::Posef {
        let rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize());
        xr::Posef {
            orientation: xr::Quaternionf {
                x: rotation.x,
                y: rotation.y,
                z: rotation.z,
                w: rotation.w,
            },
            position: xr::Vector3f { x: 0.0, y: 0.0, z: 0.0 },
        }
    }

    /// The playback controls occupy a strip of the shared surface, so a hit must
    /// land inside that strip rather than anywhere on the full panel.
    #[test]
    fn hits_map_into_the_control_strip() {
        let strip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2048.0, 420.0));
        let panel =
            CylinderPanel::facing(0.0, ORIGIN, 2.0, strip.width() / strip.height(), 55.0);

        let hit = panel
            .hit_in(pose_looking(Vec3::NEG_Z), strip)
            .expect("a forward ray should hit the strip");

        assert!((hit.x - 1024.0).abs() < 1.0, "x was {}", hit.x);
        assert!((hit.y - 210.0).abs() < 1.0, "y was {}", hit.y);
        assert!(strip.contains(hit), "hit must stay inside the strip");
    }

    /// A strip offset down the surface reports coordinates offset to match, so
    /// egui sees the pointer over the widgets actually drawn there.
    #[test]
    fn an_offset_region_shifts_the_reported_position() {
        let offset = egui::Rect::from_min_size(
            egui::pos2(0.0, 700.0),
            egui::vec2(2048.0, 420.0),
        );
        let panel =
            CylinderPanel::facing(0.0, ORIGIN, 2.0, offset.width() / offset.height(), 55.0);

        let hit = panel.hit_in(pose_looking(Vec3::NEG_Z), offset).unwrap();
        assert!((hit.y - 910.0).abs() < 1.0, "y was {}, expected 700+210", hit.y);
        assert!(offset.contains(hit));
    }

    /// Aiming past the edge of a short strip misses it, rather than clamping to
    /// an edge and producing phantom clicks on the outermost controls.
    #[test]
    fn aiming_above_a_short_strip_misses_it() {
        let strip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2048.0, 420.0));
        let panel =
            CylinderPanel::facing(0.0, ORIGIN, 2.0, strip.width() / strip.height(), 55.0);

        // The strip is only ~0.39 m tall, so 20 degrees up clears it entirely.
        let dir = Quat::from_rotation_x(20f32.to_radians()) * Vec3::NEG_Z;
        assert!(panel.hit_in(pose_looking(dir), strip).is_none());
    }
}

#[cfg(test)]
mod off_centre_tests {
    use super::*;

    fn pose_at(pos: xr::Vector3f, dir: Vec3) -> xr::Posef {
        let rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize());
        xr::Posef {
            orientation: xr::Quaternionf {
                x: rotation.x,
                y: rotation.y,
                z: rotation.z,
                w: rotation.w,
            },
            position: pos,
        }
    }

    /// A viewer away from the room origin still gets the panel centred on them.
    ///
    /// The panel used to be pinned to the stage origin regardless of where the
    /// viewer was, so anyone sitting off-centre ended up off-centre inside the
    /// cylinder — and the short control strip could miss their view entirely.
    #[test]
    fn a_seated_viewer_off_origin_is_still_centred() {
        let head = xr::Vector3f { x: 1.4, y: 1.2, z: -0.8 };
        let strip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2048.0, 420.0));
        let panel =
            CylinderPanel::facing(0.0, head, 2.0, strip.width() / strip.height(), 55.0);

        let hit = panel
            .hit_in(pose_at(head, Vec3::NEG_Z), strip)
            .expect("looking forward from the head position must hit the panel");

        assert!((hit.x - 1024.0).abs() < 1.0, "x was {}", hit.x);
        assert!((hit.y - 210.0).abs() < 1.0, "y was {}", hit.y);
    }

    /// The panel follows the viewer rather than staying at the origin.
    #[test]
    fn the_panel_moves_with_the_viewer() {
        let head = xr::Vector3f { x: 1.4, y: 1.2, z: -0.8 };
        let panel = CylinderPanel::facing(0.0, head, 2.0, 16.0 / 9.0, 55.0);
        assert!((panel.pose.position.x - head.x).abs() < 1e-5);
        assert!((panel.pose.position.z - head.z).abs() < 1e-5);
    }

    /// The controls' downward offset tips the panel rather than moving it.
    ///
    /// It used to lower the panel's position instead, which only works while
    /// "down" and "away from the eye line" are the same direction — that is,
    /// while the viewer is upright. Expressed as rotation it stays below their
    /// gaze at any recline.
    #[test]
    fn the_control_offset_tips_rather_than_translates() {
        let head = xr::Vector3f { x: 0.5, y: 1.5, z: 0.25 };
        let level = CylinderPanel::facing(0.0, head, 2.0, 5.0, 55.0);
        let dropped =
            CylinderPanel::facing_with_pitch(0.0, head, 2.0, 5.0, 55.0, 0.0, 14.0);

        // The panel stays centred on the viewer.
        assert!((dropped.pose.position.x - level.pose.position.x).abs() < 1e-5);
        assert!((dropped.pose.position.y - level.pose.position.y).abs() < 1e-5);
        assert!((dropped.pose.position.z - level.pose.position.z).abs() < 1e-5);

        // What changes is where it faces: 14 degrees below the horizon.
        let forward = quat(dropped.pose.orientation) * Vec3::NEG_Z;
        let angle = (-forward.y).asin().to_degrees();
        assert!((angle - 14.0).abs() < 0.5, "faced {angle} degrees below level");
    }
}

#[cfg(test)]
mod recline_tests {
    use super::*;

    fn head() -> xr::Vector3f {
        xr::Vector3f { x: 0.0, y: 1.2, z: 0.0 }
    }

    fn forward_of(panel: &CylinderPanel) -> Vec3 {
        quat(panel.pose.orientation) * Vec3::NEG_Z
    }

    /// Lying back tips the panel up to meet the viewer's gaze, rather than
    /// leaving it standing on the room's horizon where they cannot see it.
    #[test]
    fn reclining_tips_the_panel_upwards() {
        let upright = CylinderPanel::facing(0.0, head(), 2.0, 16.0 / 9.0, 70.0);
        let reclined = CylinderPanel::facing_with_pitch(
            0.0,
            head(),
            2.0,
            16.0 / 9.0,
            70.0,
            60f32.to_radians(),
            0.0,
        );

        assert!(forward_of(&upright).y.abs() < 1e-5, "upright faces the horizon");
        assert!(
            forward_of(&reclined).y > 0.8,
            "reclined should face upwards, got {}",
            forward_of(&reclined).y
        );
    }

    /// A ray cast the way the viewer is looking hits the centre, whatever angle
    /// they are lying at — which is the whole point of following their recline.
    #[test]
    fn a_reclined_viewer_still_hits_the_centre() {
        for recline_deg in [0.0f32, 30.0, 60.0, 85.0] {
            let recline = recline_deg.to_radians();
            let panel =
                CylinderPanel::facing_with_pitch(0.0, head(), 2.0, 16.0 / 9.0, 70.0, recline, 0.0);

            // Look along the same direction the panel was placed on.
            let dir = Quat::from_rotation_x(recline) * Vec3::NEG_Z;
            let rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize());
            let aim = xr::Posef {
                orientation: xr::Quaternionf {
                    x: rotation.x,
                    y: rotation.y,
                    z: rotation.z,
                    w: rotation.w,
                },
                position: head(),
            };

            let hit = panel
                .hit(aim, (1000.0, 500.0))
                .unwrap_or_else(|| panic!("missed the panel at {recline_deg} degrees"));
            assert!(
                (hit.x - 500.0).abs() < 2.0 && (hit.y - 250.0).abs() < 2.0,
                "at {recline_deg} degrees the hit was {hit:?}"
            );
        }
    }

    /// The controls' fixed downward offset still applies on top of recline, so
    /// the bar stays below the content rather than over it.
    #[test]
    fn the_control_offset_survives_reclining() {
        let recline = 50f32.to_radians();
        let content =
            CylinderPanel::facing_with_pitch(0.0, head(), 2.0, 5.0, 70.0, recline, 0.0);
        let bar =
            CylinderPanel::facing_with_pitch(0.0, head(), 2.0, 5.0, 70.0, recline, 14.0);
        assert!(
            forward_of(&bar).y < forward_of(&content).y,
            "the bar should sit below the content at any recline"
        );
    }
}
