# UI guide

DP0 supports one root containing column boxes, text, and buttons. The SDK
assigns anonymous root and container IDs deterministically in the lower half
of the ID space. `node!("name")` creates an app-global stable ID in the upper
half.

```rust
Tree::root(BoxNode::column([
    Text::new(node!("count"), "Count: 0"),
    Button::new(node!("increment"), "Increment"),
]))
```

Handle an activation by returning the smallest supported semantic update:

```rust
if events.activated(node!("increment")) {
    return Ok(Update::new().set_text(node!("count"), "Count: 1"));
}
Ok(Update::unchanged())
```

Names are exact UTF-8 and app-global. Duplicate names and symbolic-ID
collisions are errors. A button under a disabled box cannot be activated.
