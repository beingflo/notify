#!/usr/bin/env bash
# Shared by the mise tasks. Sourced, never executed (no executable bit, so mise
# does not expose it as a task).
#
# Helpers every task would otherwise redefine:
#   die, note             one-line failure and progress output
#   age_key               reads the sops age private key out of 1Password
#   load_deploy_secrets   decrypts the credentials into shell variables
#
# Two files, named for their contents:
#   .deploy.secrets       the plaintext credentials, gitignored, and normally
#                         absent: sops edits the ciphertext in place
#   .deploy.secrets.enc   committed sops ciphertext of those credentials
#
# Nothing here exports what it reads: values stay shell variables, so they never
# show up in the environment of child processes such as ssh.
#
# This repo has its own age key, deliberately: it holds the alert token, which
# Acta also knows, so it must not share a key with acta or marending.

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

age_item="SOPS age key - notify"

# The private half lives in 1Password and is only ever read into a variable:
# never written to disk and never exported. `deploy:secrets` uses it to decrypt,
# `deploy:edit-secrets` to hand sops an editor.
age_key() {
    command -v op >/dev/null || die "1Password CLI (op) not found (brew install 1password-cli)"
    op item get "$age_item" --reveal --fields "private key" \
        || die "could not read '$age_item' from 1Password"
}

load_deploy_secrets() {
    command -v sops >/dev/null || die "sops not found (brew install sops)"
    [[ -f .deploy.secrets.enc ]] || die ".deploy.secrets.enc not found; run: mise run deploy:edit-secrets"
    # An `if`, not `grep ... && die`: under `set -e` the AND-list would abort
    # the task on the normal path, where grep finds no match.
    if grep -q "REPLACE_WITH" .sops.yaml; then
        die ".sops.yaml still holds the placeholder recipient; put the mail age public key in it first"
    fi

    local key out
    key="$(age_key)"
    out="$(SOPS_AGE_KEY="$key" sops -d --input-type dotenv --output-type dotenv .deploy.secrets.enc)" \
        || die "sops failed to decrypt .deploy.secrets.enc; does the '$age_item' key match .sops.yaml?"
    unset key

    # `eval`, not `source <(...)`: macOS bash 3.2 sources nothing in that form.
    eval "$out"
    unset out
}
