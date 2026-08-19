# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /app
# Cache the dependency build so code-only changes rebuild in seconds.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# distroless: ~25MB total image, faster pulls on scale-out, smaller attack surface.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /app/target/release/search /usr/local/bin/search
ENV PORT=8080 HOST=0.0.0.0
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/search"]
