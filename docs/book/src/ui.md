# UI guide

The current Utility Suite contract supports one root containing column, row,
and equal-track grid boxes, text, and buttons. Named identities are app-global
and stable across reconstruction; node and command identities are separate
typed domains. Anonymous containers are deterministic and should be reserved
for static layout. Name any subtree whose identity must survive insertion,
movement, filtering, focus, resync, or replacement.

For repeated use, declare identities once:

```rust
mod ui {
    youth_sdk::ui_ids! {
        node DISPLAY = "display";
        command EQUALS = "equals";
    }
}
```

```rust
Tree::root(Column::new([
    Text::new(node!("display"), "42").align(TextAlign::End),
    Row::new([
        Button::command(command!("clear"), "C").shortcut(Shortcut::Escape),
        Button::command(command!("backspace"), "Backspace")
            .shortcut(Shortcut::Backspace),
    ]),
    Grid::columns(2, [
        Button::command(command!("one"), "1").shortcut(Shortcut::Character('1')),
        Button::command(command!("equals"), "=").shortcut(Shortcut::Enter),
    ]),
]))
```

Handle an activation by returning the smallest supported semantic update:

```rust
if events.activated(ui::EQUALS) {
    return Ok(Update::new().set_text(ui::DISPLAY, "42"));
}
Ok(Update::unchanged())
```

`Events::commanded` remains as a source-compatible alias for existing Utility
Suite applications; new code should use `activated` for both node and command
identities.

Node and command names are exact UTF-8 and app-global, but use separate typed
ID domains. Duplicate names, commands, shortcuts, and collisions are errors.
A disabled button is skipped by normal host interaction policy. Direct
semantic invocation may still reach the guest, so the guest must validate
command preconditions against its own domain state.

For dynamic collections, the SDK also provides bounded `ItemKey` identities.
An item key derives stable node and command identities from an application
namespace, nonzero item ID, and role. Named containers and explicit
`insert_child`, `remove_subtree`, and `move_child` updates express structural
changes without exposing WIT patches or numeric IDs. Todo uses these APIs for
its five-row projection; list nodes and automatic tree diffing remain outside
the current contract.

Focus and key interpretation are host-owned. Tab traverses enabled buttons in
semantic preorder without wrapping; arrows stay within the nearest applicable
row, column, or grid. Space activates focus. Enter uses the declared default
command or focused button, while Escape and Backspace use their declared
commands. The guest receives the same semantic activation as a mouse click,
never a platform keyboard event.
