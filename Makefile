.PHONY: e2e-up e2e-down e2e test

E2E_DIR := tests/e2e
E2E_ENV := $(E2E_DIR)/.env

test:
	cargo test

e2e-up:
	./$(E2E_DIR)/start.sh

e2e-down:
	./$(E2E_DIR)/stop.sh

e2e: e2e-up
	@test -f $(E2E_ENV) || (echo "missing $(E2E_ENV); e2e-up failed" >&2; exit 1)
	bash -c 'set -a && source ./$(E2E_ENV) && set +a && cargo test -- --ignored --test-threads=1'
