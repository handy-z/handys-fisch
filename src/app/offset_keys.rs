use std::sync::LazyLock;

use handy_sdk::roblox::events::{CHARACTER_KEYS, CHAT_KEYS};
use handy_sdk::roblox::offsets::OffsetKeys;
use handy_sdk::roblox::read::join_keys;

static KEYS: LazyLock<OffsetKeys> = LazyLock::new(|| {
    join_keys(
        &[&CHAT_KEYS, &CHARACTER_KEYS],
        &[
            "FakeDataModel.Pointer",
            "FakeDataModel.RealDataModel",
            "Instance.Parent",
            "Instance.NameContainer",
            "Instance.ClassDescriptor",
            "Instance.ClassName",
            "Instance.ChildrenStart",
            "Instance.ChildrenEnd",
            "GuiObject.Position",
            "GuiObject.Size",
            "GuiObject.Text",
            "GuiObject.Visible",
            "GuiObject.LayoutOrder",
            "Misc.Value",
            "Misc.StringLength",
            "BasePart.Primitive",
            "Primitive.Position",
        ],
    )
});

/// Every offset fisch reads (a superset of SDK_KEYS, CHAT_KEYS and CHARACTER_KEYS); offsets missing any are rejected.
/// Built on first use.
pub fn required_keys() -> &'static OffsetKeys {
    &KEYS
}

#[cfg(test)]
mod tests {
    use super::*;
    use handy_sdk::roblox::offsets::SDK_KEYS;

    #[test]
    fn offset_keys_cover_sdk_keys() {
        let keys = required_keys();
        for group in [&SDK_KEYS[..], &CHAT_KEYS[..], &CHARACTER_KEYS[..]] {
            for k in group {
                assert!(keys.contains(k), "{k}");
            }
        }
    }
}
