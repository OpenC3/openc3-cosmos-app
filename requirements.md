* Rust application
* Cross platform build environment that builds executables for Windows, MacOS (x86 and arm64), Linux (x86 and arm64)
* App will have multiple functions:
1. Install a working docker / docker compose environment for the native platform (if not already available)
2. Install an isolated and working Python environment for the native platform that is in a subfolder to the application 
3. Install a working OpenC3 cosmos environment that makes use of the docker compose environment in a subfolder to the application
4. Launch and monitor the OpenC3 COSMOS docker containers using docker compose
5. Should have a GUI (probably use Ice), that is on by default, but should also work as a headless app.
6. Should incorporate command line functionality that is the equivalent of all the functionality in openc3.sh
7. Launch and keep alive host-side Python microservices that drive interfaces which must run on the host (serial, USB, local-only TCP). Implemented — see `src/operator.rs` and `docs/bridge-architecture.md`.
8. Communicate with the host Python microservices over Iroh (QUIC-based P2P), as a control-plane client of the COSMOS bridge hub. Implemented — see `src/bridge.rs`, `src/enroll.rs`, and `docs/bridge-architecture.md`.




