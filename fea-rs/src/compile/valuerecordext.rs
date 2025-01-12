//! Extra helper methods on ValueRecord

use write_fonts::tables::gpos::{ValueFormat, ValueRecord};

pub(crate) trait ValueRecordExt {
    fn clear_zeros(self) -> Self;
    fn for_pair_pos(self, in_vert_feature: bool) -> Self;
    fn is_all_zeros(&self) -> bool;
}

impl ValueRecordExt for ValueRecord {
    fn clear_zeros(mut self) -> Self {
        if self.x_placement == Some(0) {
            self.x_placement = None;
        }

        if self.y_placement == Some(0) {
            self.y_placement = None;
        }

        if self.x_advance == Some(0) {
            self.x_advance = None;
        }

        if self.y_advance == Some(0) {
            self.y_advance = None;
        }

        self
    }

    /// `true` if we are not null, but our set values are all 0
    fn is_all_zeros(&self) -> bool {
        let device_mask = ValueFormat::X_PLACEMENT_DEVICE
            | ValueFormat::Y_PLACEMENT_DEVICE
            | ValueFormat::X_ADVANCE_DEVICE
            | ValueFormat::Y_ADVANCE_DEVICE;

        let format = self.format();
        if format.is_empty() || format.intersects(device_mask) {
            return false;
        }
        let all_values = [
            self.x_placement,
            self.y_placement,
            self.x_advance,
            self.y_advance,
        ];
        all_values.iter().all(|v| v.unwrap_or_default() == 0)
    }

    // Modify this value record for the special requirements of pairpos lookups
    //
    // In pair pos tables, if a value record is all zeros (but not null) then
    // we interpret it as a having a single zero advance in the x/y direction,
    // depending on context.
    fn for_pair_pos(self, in_vert_feature: bool) -> Self {
        if !self.is_all_zeros() {
            return self.clear_zeros();
        }
        let mut out = self.clear_zeros();
        if in_vert_feature {
            out.y_advance = Some(0);
        } else {
            out.x_advance = Some(0);
        }
        out
    }
}
