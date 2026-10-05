.DEFAULT_GOAL := help

CARGO_TOML := Cargo.toml

define get_version
$(shell awk '/^\[workspace.package\]/{in_workspace_package=1; next} /^\[/{in_workspace_package=0} in_workspace_package && /^version = /{gsub(/"/, "", $$3); print $$3; exit}' $(CARGO_TOML))
endef

define update_version
	perl -0pi -e 's/(\[workspace\.package\]\nversion = ")[^"]+(")/$${1}$(1)$${2}/' $(CARGO_TOML)
endef

.PHONY: help build check check-core-wasm check-execution-wasm test clean lint release release-tag npm-package npm-pack-dry-run npm-pack update-patch update-minor update-major run-local run-controller run-cormilo-agent

LOCAL_ARGS ?=
CONTROLLER_ARGS ?=
NPM_CACHE ?= /tmp/taskr-npm-cache

help:
	@printf 'taskr make targets\n'
	@printf '\nBuild/test:\n'
	@printf '  make build             Debug-build the full Cargo workspace\n'
	@printf '  make check             Type-check the full Cargo workspace\n'
	@printf '  make check-core-wasm   Check taskr-core and consumers for Workers Wasm\n'
	@printf '  make check-execution-wasm  Check Herdr/container and companion ports for Wasm\n'
	@printf '  make test              Run workspace tests\n'
	@printf '  make lint              Run clippy across workspace targets\n'
	@printf '  make release           Release-build the full Cargo workspace\n'
	@printf '\nRelease publishing:\n'
	@printf '  make update-patch      Bump workspace patch version\n'
	@printf '  make update-minor      Bump workspace minor version\n'
	@printf '  make update-major      Bump workspace major version\n'
	@printf '  make release-tag       Tag v$$(workspace.package.version) and push it to trigger GitHub release\n'
	@printf '  make npm-package       Build current-platform npm archive under npm/taskr/artifacts\n'
	@printf '  make npm-pack-dry-run  Build package, then inspect npm pack contents\n'
	@printf '  make npm-pack          Build package, then run npm pack in npm/taskr\n'
	@printf '\nRun locally:\n'
	@printf '  make run-local         Run taskr controller against the local Herdr server\n'
	@printf '  make run-controller    Run taskr controller\n'
	@printf '  make run-cormilo-agent Run the local Cormilo taskr orchestration agent\n'
	@printf '\nHerdr is an external terminal/agent engine; TASKR reaches it via --herdr-bin.\n'

# Default workspace build (debug)
build:
	cargo build --workspace

# Check the full workspace without producing binaries
check:
	cargo check --workspace

# Install the target first: rustup target add wasm32-unknown-unknown
check-core-wasm:
	cargo check -p taskr-core --locked --target wasm32-unknown-unknown --features wasm-js --all-targets

# Portable ports must not pick up native runner/catalog features.
check-execution-wasm:
	cargo check -p taskr-herdr -p taskr-environment --no-default-features --locked --target wasm32-unknown-unknown --all-targets

# Release build
release:
	cargo build --workspace --release

update-patch:
	@echo "Updating patch version..."
	$(eval CURRENT_VERSION := $(call get_version))
	$(eval NEW_VERSION := $(shell echo $(CURRENT_VERSION) | awk -F. '{$$3=$$3+1} 1' OFS=.))
	$(call update_version,$(NEW_VERSION))
	@echo "Version updated from $(CURRENT_VERSION) to $(NEW_VERSION)"

update-minor:
	@echo "Updating minor version..."
	$(eval CURRENT_VERSION := $(call get_version))
	$(eval NEW_VERSION := $(shell echo $(CURRENT_VERSION) | awk -F. '{$$2=$$2+1; $$3=0} 1' OFS=.))
	$(call update_version,$(NEW_VERSION))
	@echo "Version updated from $(CURRENT_VERSION) to $(NEW_VERSION)"

update-major:
	@echo "Updating major version..."
	$(eval CURRENT_VERSION := $(call get_version))
	$(eval NEW_VERSION := $(shell echo $(CURRENT_VERSION) | awk -F. '{$$1=$$1+1; $$2=0; $$3=0} 1' OFS=.))
	$(call update_version,$(NEW_VERSION))
	@echo "Version updated from $(CURRENT_VERSION) to $(NEW_VERSION)"

release-tag:
	@echo "Creating release tag from current branch..."
	$(eval VERSION := $(call get_version))
	$(eval CURRENT_BRANCH := $(shell git branch --show-current))
	@if [ "$(CURRENT_BRANCH)" != "main" ]; then \
		echo "ERROR: release-tag must be run from main. Current branch: $(CURRENT_BRANCH)"; \
		exit 1; \
	fi
	@if ! git diff-index --quiet HEAD --; then \
		echo "ERROR: working directory has uncommitted changes."; \
		exit 1; \
	fi
	cargo test --workspace
	cargo build --release --bin taskr
	git tag -a v$(VERSION) -m "Release version v$(VERSION)"
	git push origin v$(VERSION)
	@echo "Release v$(VERSION) tagged. GitHub Actions will build and publish artifacts."

npm-package:
	./scripts/npm-package.sh

npm-pack-dry-run: npm-package
	cd npm/taskr && npm --cache $(NPM_CACHE) pack --dry-run

npm-pack: npm-package
	cd npm/taskr && npm --cache $(NPM_CACHE) pack

# Run workspace tests
test:
	cargo test --workspace

# Run clippy across workspace targets
lint:
	cargo clippy --workspace --all-targets

# Run the controller against the local Herdr server
run-local:
	cargo run -- controller $(LOCAL_ARGS)

# Run only the MCP controller entrypoint
run-controller:
	cargo run -- controller $(CONTROLLER_ARGS)

run-cormilo-agent:
	$(MAKE) -C taskr-cormilo-agent start

# Clean build artifacts
clean:
	cargo clean
