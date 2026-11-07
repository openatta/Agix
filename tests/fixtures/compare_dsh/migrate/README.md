# app

Service clients, one module per upstream service under `app/services/`. Each module's
`load()` gathers what that service exposes. All HTTP goes through `app/util.py`.

`tests/run_tests.py` runs every `load()` against a stub of `app.util`, so it needs no
network.
