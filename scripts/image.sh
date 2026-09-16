dist build --target x86_64-unknown-linux-gnu
podman build --platform linux/amd64 -f deploy/Dockerfile -t coral:latest .
