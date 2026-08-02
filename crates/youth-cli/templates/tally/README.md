# {{display_name}}

A Youth application generated from the Youth 0.0.8 Tally template.

The generated application ID is `{{app_id}}`. Youth uses that stable identity
to choose the application's durable state, so restarting or rebuilding this
project does not accidentally share state with another application. Pass
`youth new <directory> --id <custom-id>` when creating a project if you need a
different identity.

```text
youth check
youth test
youth dev
youth build --release
```

The vendored WIT directory is an inspectable protocol snapshot. Rust bindings
and export plumbing come only from the exact `youth-sdk` revision in
`Youth.lock`.
