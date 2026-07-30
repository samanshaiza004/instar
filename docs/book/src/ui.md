# UI guide

The current Utility Suite contract supports one root containing column, row,
and equal-track grid boxes, text, and buttons. The SDK
assigns anonymous root and container IDs deterministically in the lower half
of the ID space. `node!("name")` creates an app-global stable ID in the upper
half.

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
if events.commanded(command!("equals")) {
    return Ok(Update::new().set_text(node!("display"), "42"));
}
Ok(Update::unchanged())
```

Node and command names are exact UTF-8 and app-global, but use separate typed
ID domains. Duplicate names, commands, shortcuts, and collisions are errors.
A button under a disabled box cannot be activated.

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
