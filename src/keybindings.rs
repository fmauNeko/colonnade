//! Key binding registration that resolves the `secondary-` modifier alias for the host OS.

use std::rc::Rc;

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate};

/// Binds `keystrokes` (space-separated chords, `secondary-` allowed) to `action`, optionally
/// scoped to a key `context`.
///
/// A binding that cannot be parsed is reported on stderr and skipped, so a typo cannot abort
/// startup.
pub fn bind(cx: &mut App, keystrokes: &str, action: impl Action, context: Option<&str>) {
    let predicate = match context.map(KeyBindingContextPredicate::parse).transpose() {
        Ok(predicate) => predicate.map(Rc::new),
        Err(error) => {
            eprintln!("invalid key context for {keystrokes:?}: {error}");
            return;
        }
    };
    match KeyBinding::load(keystrokes, Box::new(action), predicate, false, None, cx) {
        Ok(binding) => cx.bind_keys([binding]),
        Err(error) => eprintln!("invalid key binding {keystrokes:?}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{App, Keystroke, TestAppContext, actions};

    use super::bind;

    actions!(keybindings_test, [Probe]);

    fn resolved(cx: &App, keystrokes: &str) -> usize {
        let os = cx.platform_info().os;
        let input: Vec<Keystroke> = keystrokes
            .split(' ')
            .map(|chord| Keystroke::parse_with_os(chord, os).expect("valid keystroke"))
            .collect();
        cx.all_bindings_for_input(&input).len()
    }

    #[gpui_kit::test]
    fn bind_resolves_secondary_alias_for_the_host_os(cx: &mut TestAppContext) {
        // Given an app with no binding on secondary-f12
        cx.update(|cx| assert_eq!(resolved(cx, "secondary-f12"), 0));

        // When binding secondary-f12 and a two-chord sequence through the helper
        cx.update(|cx| {
            bind(cx, "secondary-f12", Probe, Some("Workspace"));
            bind(cx, "secondary-k secondary-f12", Probe, None);
        });

        // Then both are registered for the host-resolved keystrokes
        cx.update(|cx| {
            assert_eq!(resolved(cx, "secondary-f12"), 1);
            assert_eq!(resolved(cx, "secondary-k secondary-f12"), 1);
        });
    }

    #[gpui_kit::test]
    fn bind_skips_invalid_input_and_keeps_working(cx: &mut TestAppContext) {
        // Given an invalid keystroke and an invalid context, both bound through the helper
        cx.update(|cx| {
            bind(cx, "not-a-key-", Probe, None);
            bind(cx, "f11", Probe, Some("(("));
        });

        // Then neither registered a binding
        cx.update(|cx| assert_eq!(resolved(cx, "f11"), 0));

        // When binding a valid shortcut afterwards
        cx.update(|cx| bind(cx, "f11", Probe, None));

        // Then it is registered
        cx.update(|cx| assert_eq!(resolved(cx, "f11"), 1));
    }
}
