//! Raw FFI bindings to the macOS Accessibility API (AXUIElement).
//!
//! We call the C-level AX API directly rather than using a crate wrapper,
//! because most available crates are incomplete or unmaintained.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]

use core_foundation::{
    array::CFArrayRef,
    base::{CFRelease, CFRetain, CFTypeID, CFTypeRef},
    string::CFStringRef,
};
use std::os::raw::{c_int, c_void};

// ── AXUIElement opaque type ──────────────────────────────────────────────────

#[repr(C)]
pub struct __AXUIElement(c_void);
pub type AXUIElementRef = *mut __AXUIElement;

// ── AXError ──────────────────────────────────────────────────────────────────

pub type AXError = c_int;
pub const kAXErrorSuccess: AXError = 0;
pub const kAXErrorFailure: AXError = -25200;
pub const kAXErrorInvalidUIElement: AXError = -25202;
pub const kAXErrorAttributeUnsupported: AXError = -25205;
pub const kAXErrorNotImplemented: AXError = -25206;
pub const kAXErrorParameterizedAttributeUnsupported: AXError = -25207;
pub const kAXErrorNoValue: AXError = -25212;
pub const kAXErrorAPIDisabled: AXError = -25211;

// ── AXValue opaque type ──────────────────────────────────────────────────────

#[repr(C)]
pub struct __AXValue(c_void);
pub type AXValueRef = *mut __AXValue;

pub type AXValueType = c_int;
pub const kAXValueCGPointType: AXValueType = 1;
pub const kAXValueCGSizeType: AXValueType = 2;
pub const kAXValueCGRectType: AXValueType = 3;
pub const kAXValueCFRangeType: AXValueType = 4;
pub const kAXValueAXErrorType: AXValueType = 5;
pub const kAXValueIllegalType: AXValueType = 1_000;

// ── Link to AXUIElement functions ────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyMultipleAttributeValues(
        element: AXUIElementRef,
        attributes: CFArrayRef,
        options: usize,
        values: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeNames(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> AXError;
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    /// Bound one AX IPC request. The timeout is in seconds and applies to
    /// subsequent messaging for this element. This is cooperative with the
    /// walk deadline: calls already in flight still return when this limit or
    /// the system's own AX messaging timeout is reached.
    pub fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout: f32) -> AXError;
    pub fn AXUIElementGetTypeID() -> CFTypeID;
    pub fn AXIsProcessTrusted() -> bool;
    /// `AXIsProcessTrustedWithOptions(options)` — when called with
    /// `{kAXTrustedCheckOptionPrompt: true}` raises the system Accessibility
    /// prompt if the process isn't already trusted.  Returns the post-prompt
    /// trust state (may still be false if the user dismissed the prompt).
    pub fn AXIsProcessTrustedWithOptions(
        options: core_foundation::dictionary::CFDictionaryRef,
    ) -> bool;

    /// Private SPI: maps an AX window element to its CGWindowID.
    /// Stable since macOS 10.9; used by yabai, Hammerspoon, Accessibility Inspector.
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}

/// Hit-test one process's accessibility tree at a screen point. The returned
/// element is retained and must be released by the caller.
pub unsafe fn element_at_screen_position(pid: i32, x: f64, y: f64) -> Option<AXUIElementRef> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    let mut element = std::ptr::null_mut();
    let error = AXUIElementCopyElementAtPosition(application, x as f32, y as f32, &mut element);
    CFRelease(application as CFTypeRef);
    (error == kAXErrorSuccess && !element.is_null()).then_some(element)
}

// ── AXValue functions ────────────────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXValueGetTypeID() -> CFTypeID;
    pub fn AXValueGetType(value: AXValueRef) -> AXValueType;
    pub fn AXValueGetValue(
        value: AXValueRef,
        the_type: AXValueType,
        value_ptr: *mut c_void,
    ) -> bool;
}

// ── Helper functions ──────────────────────────────────────────────────────────

use core_foundation::{array::CFArray, base::TCFType, string::CFString as CFStr};

/// Copy a string attribute from an AX element. Returns `None` on any error.
pub unsafe fn copy_string_attr(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let cf_string_type_id = CFStr::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_string_type_id {
        CFRelease(value);
        return None;
    }
    let s = CFStr::wrap_under_create_rule(value as _);
    Some(s.to_string())
}

/// The small, scalar descriptor surface needed to render and route an AX node.
/// The values are decoded while the temporary AX array is alive, so callers do
/// not have to manage Core Foundation retains for each batch member.
#[derive(Debug, Default, Clone)]
pub struct AXDescriptorStrings {
    pub position: Option<(f64, f64)>,
    pub size: Option<(f64, f64)>,
    pub role: Option<String>,
    pub label: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub role_description: Option<String>,
    pub placeholder: Option<String>,
    pub enabled: Option<bool>,
    pub selected: Option<bool>,
    pub focused: Option<bool>,
    pub expanded: Option<bool>,
    pub hidden: Option<bool>,
    pub complete: bool,
}

const DESCRIPTOR_ATTRIBUTE_NAMES: [&str; 16] = [
    "AXPosition",
    "AXSize",
    "AXRole",
    "AXTitle",
    "AXLabel",
    "AXValue",
    "AXDescription",
    "AXIdentifier",
    "AXHelp",
    "AXRoleDescription",
    "AXPlaceholderValue",
    "AXEnabled",
    "AXSelected",
    "AXFocused",
    "AXExpanded",
    "AXHidden",
];

/// Read the descriptor attributes in one AX IPC request when the target
/// implements `AXUIElementCopyMultipleAttributeValues`. Older or partial AX
/// implementations return an unsupported/malformed response; those fall back
/// to the existing single-attribute helpers. Missing optional values remain
/// sparse rather than invalidating the entire node.
pub unsafe fn copy_descriptor_strings(element: AXUIElementRef) -> AXDescriptorStrings {
    let names: Vec<CFStr> = DESCRIPTOR_ATTRIBUTE_NAMES
        .iter()
        .map(|name| CFStr::new(name))
        .collect();
    let names_array = CFArray::from_CFTypes(&names);
    let mut raw_values: CFArrayRef = std::ptr::null();
    let error = AXUIElementCopyMultipleAttributeValues(
        element,
        names_array.as_concrete_TypeRef(),
        0,
        &mut raw_values,
    );
    if error == kAXErrorSuccess && !raw_values.is_null() {
        let values = CFArray::<CFTypeRef>::wrap_under_create_rule(raw_values as _);
        let value_count = values.len() as usize;
        if value_count == DESCRIPTOR_ATTRIBUTE_NAMES.len() {
            let mut decoded = AXDescriptorStrings {
                complete: true,
                ..Default::default()
            };
            for index in 0..value_count {
                let Some(value) = values.get(index) else {
                    continue;
                };
                let value = *value;
                if value.is_null() {
                    continue;
                }
                if embedded_ax_error(value).is_some() {
                    // AXRole is required to identify a node. Position and
                    // size are required for visibility/pruning when present;
                    // an embedded error means the batch did not complete.
                    if matches!(index, 0 | 1 | 2) {
                        decoded.complete = false;
                    }
                    continue;
                }
                match index {
                    0 => decoded.position = cf_point_value(value, kAXValueCGPointType),
                    1 => decoded.size = cf_point_value(value, kAXValueCGSizeType),
                    2 => decoded.role = cf_string_value(value),
                    3 => decoded.title = cf_string_value(value),
                    4 => decoded.label = cf_string_value(value),
                    5 => decoded.value = cf_scalar_string_value(value),
                    6 => decoded.description = cf_string_value(value),
                    7 => decoded.identifier = cf_string_value(value),
                    8 => decoded.help = cf_string_value(value),
                    9 => decoded.role_description = cf_string_value(value),
                    10 => decoded.placeholder = cf_string_value(value),
                    11 => decoded.enabled = cf_bool_value(value),
                    12 => decoded.selected = cf_bool_value(value),
                    13 => decoded.focused = cf_bool_value(value),
                    14 => decoded.expanded = cf_bool_value(value),
                    15 => decoded.hidden = cf_bool_value(value),
                    _ => unreachable!(),
                }
            }
            // AXRole is the only descriptor required to identify a node. A
            // sparse role means the batch was incomplete, so use the reliable
            // single-attribute path instead of manufacturing AXUnknown.
            if decoded.complete && decoded.role.is_some() {
                return decoded;
            }
        }
        // `values` releases the returned array here. No child values were
        // retained, so there is no per-member cleanup on the fallback path.
    }

    AXDescriptorStrings {
        position: None,
        size: None,
        role: copy_string_attr(element, "AXRole"),
        label: copy_string_attr(element, "AXLabel"),
        title: copy_string_attr(element, "AXTitle"),
        value: copy_stringified_attr(element, "AXValue"),
        description: copy_string_attr(element, "AXDescription"),
        identifier: copy_string_attr(element, "AXIdentifier"),
        help: copy_string_attr(element, "AXHelp"),
        role_description: copy_string_attr(element, "AXRoleDescription"),
        placeholder: copy_string_attr(element, "AXPlaceholderValue"),
        enabled: copy_bool_attr(element, "AXEnabled"),
        selected: copy_bool_attr(element, "AXSelected"),
        focused: copy_bool_attr(element, "AXFocused"),
        expanded: copy_bool_attr(element, "AXExpanded"),
        hidden: copy_bool_attr(element, "AXHidden"),
        complete: false,
    }
}

unsafe fn embedded_ax_error(value: CFTypeRef) -> Option<AXError> {
    if core_foundation::base::CFGetTypeID(value) != AXValueGetTypeID()
        || AXValueGetType(value as AXValueRef) != kAXValueAXErrorType
    {
        return None;
    }
    let mut error = kAXErrorFailure;
    AXValueGetValue(
        value as AXValueRef,
        kAXValueAXErrorType,
        &mut error as *mut _ as *mut c_void,
    )
    .then_some(error)
}

unsafe fn cf_string_value(value: CFTypeRef) -> Option<String> {
    (core_foundation::base::CFGetTypeID(value) == CFStr::type_id())
        .then(|| CFStr::wrap_under_get_rule(value as _).to_string())
}

unsafe fn cf_point_value(value: CFTypeRef, value_type: AXValueType) -> Option<(f64, f64)> {
    if core_foundation::base::CFGetTypeID(value) != AXValueGetTypeID()
        || AXValueGetType(value as AXValueRef) != value_type
    {
        return None;
    }
    #[repr(C)]
    struct Pair {
        first: f64,
        second: f64,
    }
    let mut pair = Pair {
        first: 0.0,
        second: 0.0,
    };
    AXValueGetValue(
        value as AXValueRef,
        value_type,
        &mut pair as *mut _ as *mut c_void,
    )
    .then_some((pair.first, pair.second))
}

unsafe fn cf_scalar_string_value(value: CFTypeRef) -> Option<String> {
    use core_foundation::{boolean::CFBoolean, number::CFNumber};
    if let Some(value) = cf_string_value(value) {
        return Some(value);
    }
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFNumber::type_id() {
        return CFNumber::wrap_under_get_rule(value as _)
            .to_f64()
            .map(|number| {
                if number.fract() == 0.0 {
                    format!("{number:.0}")
                } else {
                    number.to_string()
                }
            });
    }
    if type_id == CFBoolean::type_id() {
        return Some(bool::from(CFBoolean::wrap_under_get_rule(value as _)).to_string());
    }
    None
}

unsafe fn cf_bool_value(value: CFTypeRef) -> Option<bool> {
    use core_foundation::{boolean::CFBoolean, number::CFNumber};
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        return Some(bool::from(CFBoolean::wrap_under_get_rule(value as _)));
    }
    if type_id == CFNumber::type_id() {
        return CFNumber::wrap_under_get_rule(value as _)
            .to_f64()
            .map(|n| n != 0.0);
    }
    None
}

/// Read a boolean-valued AX attribute (CFBoolean or numeric 0/1).
pub unsafe fn copy_bool_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let decoded = cf_bool_value(value);
    CFRelease(value);
    decoded
}

/// Copy any scalar AX attribute and render it as text. Unlike
/// [`copy_string_attr`], this preserves numeric/boolean AXValue payloads (for
/// example sliders, steppers, and Calculator's display). Uncommon value types
/// are omitted because Core Foundation descriptions may contain process-local
/// pointer addresses and therefore are not deterministic.
pub unsafe fn copy_stringified_attr(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    use core_foundation::{boolean::CFBoolean, number::CFNumber, string::CFString};

    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }

    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFString::type_id() {
        return Some(CFString::wrap_under_create_rule(value as _).to_string());
    }
    if type_id == CFNumber::type_id() {
        let number = CFNumber::wrap_under_create_rule(value as _);
        return number.to_f64().map(|number| {
            if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            }
        });
    }
    if type_id == CFBoolean::type_id() {
        let boolean = bool::from(CFBoolean::wrap_under_create_rule(value as _));
        return Some(boolean.to_string());
    }

    CFRelease(value);
    None
}

/// Copy an AXValue(CFRange) attribute such as `AXVisibleCharacterRange`.
pub unsafe fn copy_range_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<core_foundation::base::CFRange> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != AXValueGetTypeID()
        || AXValueGetType(value as AXValueRef) != kAXValueCFRangeType
    {
        CFRelease(value);
        return None;
    }
    let mut range = core_foundation::base::CFRange::init(0, 0);
    let ok = AXValueGetValue(
        value as AXValueRef,
        kAXValueCFRangeType,
        &mut range as *mut _ as *mut c_void,
    );
    CFRelease(value);
    ok.then_some(range)
}

/// Copy a numeric attribute from an AX element as an `f64`. Returns `None` on
/// any error or if the attribute is not a `CFNumber`. SwiftUI sliders expose a
/// readable numeric `AXValue` even when that value is not settable — this lets
/// the stepping fallback read the control's current position for feedback.
pub unsafe fn copy_number_attr(element: AXUIElementRef, attr_name: &str) -> Option<f64> {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let cf_number_type_id = CFNumber::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_number_type_id {
        CFRelease(value);
        return None;
    }
    let n = CFNumber::wrap_under_create_rule(value as _);
    n.to_f64()
}

/// Get the action names for an AX element.
pub unsafe fn copy_action_names(element: AXUIElementRef) -> Vec<String> {
    let mut names: CFArrayRef = std::ptr::null_mut();
    let err = AXUIElementCopyActionNames(element, &mut names);
    if err != kAXErrorSuccess || names.is_null() {
        return vec![];
    }
    // Use CFArray<CFStr> (the typed wrapper) to satisfy FromVoid bound.
    let arr = CFArray::<CFStr>::wrap_under_create_rule(names);
    (0..arr.len())
        .filter_map(|i| {
            let cf = arr.get(i)?;
            Some(cf.to_string())
        })
        .collect()
}

/// Read the on-screen center of an AX element (AXPosition + AXSize → center).
/// Returns `(cx, cy)` in screen coordinates, or `None` if either attribute
/// is unavailable or the element has zero size.
pub unsafe fn element_screen_center(element: AXUIElementRef) -> Option<(f64, f64)> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some((pos.x + sz.w / 2.0, pos.y + sz.h / 2.0))
}

/// Read the on-screen bounding rect of an AX element.
/// Returns `[x, y, width, height]` in screen coordinates (top-left origin), or `None`.
pub unsafe fn element_screen_rect(element: AXUIElementRef) -> Option<[f64; 4]> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some([pos.x, pos.y, sz.w, sz.h])
}

/// Get the focused UI element of a running application by pid.
/// Returns a retained `AXUIElementRef` that the caller must release, or `None`.
pub unsafe fn focused_element_of_pid(pid: i32) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let attr = CFStr::new("AXFocusedUIElement");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(app, attr.as_concrete_TypeRef(), &mut value);
    CFRelease(app as CFTypeRef);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let ax_type_id = AXUIElementGetTypeID();
    if core_foundation::base::CFGetTypeID(value) != ax_type_id {
        CFRelease(value);
        return None;
    }
    // Already retained by CopyAttributeValue — hand the raw pointer to the caller.
    Some(value as AXUIElementRef)
}

/// Get the children of an AX element.
pub unsafe fn copy_children(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    copy_element_array_attr(element, "AXChildren")
}

/// Copy an AX attribute whose value is an array of accessibility elements.
///
/// This is also used for bounded collection traversal (`AXVisibleRows` /
/// `AXVisibleChildren`) so a window snapshot does not enumerate thousands of
/// offscreen rows before reaching the controls visible in the screenshot.
pub unsafe fn copy_element_array_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Vec<AXUIElementRef> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return vec![];
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return vec![];
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                // Retain so we own it — caller is responsible for releasing.
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect()
}

/// Copy an AX element-valued attribute. The returned element is retained and
/// must be released by the caller.
pub unsafe fn copy_element_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<AXUIElementRef> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != AXUIElementGetTypeID() {
        CFRelease(value);
        return None;
    }
    Some(value as AXUIElementRef)
}

/// Perform an AX action using a string attribute name.
pub unsafe fn perform_action(element: AXUIElementRef, action_name: &str) -> AXError {
    let action = CFStr::new(action_name);
    AXUIElementPerformAction(element, action.as_concrete_TypeRef())
}

/// Set an AX attribute to a CFString value.
pub unsafe fn set_string_attr(element: AXUIElementRef, attr_name: &str, value: &str) -> AXError {
    let attr = CFStr::new(attr_name);
    let cf_value = CFStr::new(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFNumber (double) value. Numeric controls — most
/// notably `AXSlider` (NSSlider) and `AXStepper` — expose a numeric `AXValue`
/// reject a `CFString` write — `-25200` (kAXErrorFailure, observed live on a
/// SwiftUI `AXSlider`) or `-25201` (kAXErrorIllegalArgument); only a `CFNumber`
/// is accepted. Text fields, by contrast, take a `CFString`.
pub unsafe fn set_number_attr(element: AXUIElementRef, attr_name: &str, value: f64) -> AXError {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let cf_value = CFNumber::from(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFBoolean true value.
pub unsafe fn set_bool_attr_true(element: AXUIElementRef, attr_name: &str) -> AXError {
    use core_foundation::boolean::CFBoolean;
    let attr = CFStr::new(attr_name);
    let cf_true = CFBoolean::true_value();
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_true.as_CFTypeRef())
}

/// Signal to a Chromium/Electron application root that a real assistive client
/// is present so it materializes its full web-content accessibility tree.
///
/// Returns `true` when an attribute write was accepted — meaning the app was
/// flipped from "tree off" to "tree building" and the caller should let the
/// tree settle before walking. Returns `false` when the app does not support
/// either attribute (native Cocoa apps such as Finder / Calculator / TextEdit),
/// in which case no settle delay is warranted.
///
/// `AXManualAccessibility` is the modern opt-in with no screen-reader side
/// effects; `AXEnhancedUserInterface` is the legacy fallback some Electron
/// builds expose instead (the modern attribute returns
/// `kAXErrorAttributeUnsupported` on those builds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityOptIn {
    NotAccepted,
    ManualAccessibility,
    EnhancedUserInterface,
}

pub unsafe fn enable_chromium_accessibility(app_element: AXUIElementRef) -> AccessibilityOptIn {
    let manual = set_bool_attr_true(app_element, "AXManualAccessibility");
    if manual == kAXErrorSuccess {
        return AccessibilityOptIn::ManualAccessibility;
    }
    if manual != kAXErrorAttributeUnsupported {
        // A transient error (e.g. timeout / app busy) rather than a hard
        // "this app has no such attribute" — don't bother with the legacy
        // fallback, and don't claim enablement happened.
        return AccessibilityOptIn::NotAccepted;
    }
    if set_bool_attr_true(app_element, "AXEnhancedUserInterface") == kAXErrorSuccess {
        AccessibilityOptIn::EnhancedUserInterface
    } else {
        AccessibilityOptIn::NotAccepted
    }
}

/// Get the CGWindowID of an AX window element via the private `_AXUIElementGetWindow` SPI.
/// Returns `None` if the element is not a composited window.
pub unsafe fn ax_get_window_id(element: AXUIElementRef) -> Option<u32> {
    let mut wid: u32 = 0;
    let err = _AXUIElementGetWindow(element, &mut wid);
    if err == kAXErrorSuccess && wid != 0 {
        Some(wid)
    } else {
        None
    }
}

/// Read the `AXWindows` attribute of an application element.
/// Unlike `AXChildren`, this returns the window list regardless of whether
/// the app is frontmost. Returns a Vec of retained AXUIElementRefs.
pub unsafe fn copy_ax_windows(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    let attr = CFStr::new("AXWindows");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return vec![];
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return vec![];
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect()
}
