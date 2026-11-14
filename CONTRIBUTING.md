# Guide de Contribution — `dtnpm-cla-ltp-kiss`

Ce guide présente les règles de développement et les exigences de sécurité pour le CLA LTP et KISS de `dtnpm-cla-ltp-kiss`.

---

## 1. Principes Fondamentaux

- **Zéro Panic / Zéro Unwrap** : Aucun appel `.unwrap()` ou `.expect()` en code de production.
- **Sécurité et Bornage des Ressources** :
  - L'authentification des pairs UDP est obligatoire par défaut (`static_peers`), avec opt-in explicite (`--allow-unauthenticated-udp`) réservé aux usages volontaires.
  - Tout réassemblage de segments réseau doit être borné en mémoire et valider la contiguïté stricte avant transmission au BPA.
- **Traçabilité Architecturale (ADR)** :
  - Tout ajout ou ajustement protocolaire doit faire l'objet d'un fichier ADR sous `adr/` et être indexé dans `adr/README.md`.

---

## 2. Vérification Locale

```bash
cargo fmt --all -- --check
cargo clippy --offline --workspace --lib --bins -- -D warnings -D clippy::unwrap_used -D clippy::panic
cargo test --offline --all-features
./scripts/check-security.sh --strict
```
