# Agent Guide — `dtnpm-cla-ltp-kiss`

## 🎯 Mission
Fournir l'adaptateur de couche de convergence (CLA) LTP (Licklider Transmission Protocol - RFC 5326 / RFC 7122) et KISS pour l'écosystème DTN Postmaster, comprenant le moteur d'état protocolaire (`hardy-ltp`), l'encapsulation pour TNCs radio (Direwolf, packet radio AX.25) et le démon autonome gRPC.

---

## 🛡️ Référentiels et Exigences de Sécurité

### 1. Cyber Resilience Act (CRA) & Sécurité par Défaut
- **Authentification des Pairs UDP par Défaut** :
  - Par défaut, les datagrammes UDP ne sont acceptés que s'ils proviennent d'adresses enregistrées dans `static_peers`.
  - La table dynamique des correspondances `engine_id -> SocketAddr` est strictement bornée à 256 pairs actifs pour prévenir l'épuisement mémoire.
- **Opt-in Explicite pour les Déploiements Libres / Labo** :
  - L'acceptation de paquets UDP provenant d'adresses inconnues ou la migration dynamique d'adresse IP (`allow_ip_migration`) N'EST ACTIVÉE QUE si l'utilisateur le demande EXPRESSÉMENT via le drapeau CLI `--allow-unauthenticated-udp` ou dans la configuration (`allow_unauthenticated_udp = true`).
  - Utile et autorisé pour le développement, les tests ou les réseaux déjà sécurisés au niveau transport (WireGuard, tunnels chiffrés).
- **Supply-Chain & VEX** :
  - Audit `cargo-audit` et `cargo-deny` sans échec ; VEX `not_affected` consigné pour `RUSTSEC-2026-0459`.
  - Actions CI épinglées par SHA-1 de 40 caractères.
- **Politique de sécurité** : Consignée dans [SECURITY.md](SECURITY.md).

### 2. Robustesse Protocolaire & Bornage Mémoire
- **Réassemblage Green Bounded** :
  - La réagrégation des fragments Green non fiables utilise une table ordonnée `BTreeMap` stockant les tronçons réels reçus, sans préallouer de vecteur géant (prévention des attaques d'amplification OOM de 1 Gio).
  - Validation stricte de contiguïté : un bundle n'est délivré au BPA que si la somme des longueurs des fragments continus couvre exactement la taille déclarée.
  - Plafond de mémoire global imposé à 32 Mio par session.
- **Moteur LTP (`hardy-ltp`)** :
  - Limite stricte de 1 024 sessions de réception (`RecvSession`) simultanées.
  - Coalescence des blocs Red épars (`sparse_red`) pour éviter les fuites de quota.
  - Annulation propre des minuteurs de retransmission avec notification d'annulation (`record_cancelled`).
- **Framing KISS** :
  - Utilisation d'une balise de bourrage d'extension conforme `0xFF` (évitant toute collision avec l'octet de fin `0x00`).
  - Découpage de trame sécurisé (`cp_end.max(1)`).

---

## 🦀 Standards d'Ingénierie Rust

1. **Règle Zéro Panic / Zéro Unwrap** :
   - Tout le code (`src/` et `ltp/src/`) doit être 100% exempt de `.unwrap()`, `.expect()` et de macro `panic!`.
   - Linter strict : `-D warnings -D clippy::unwrap_used -D clippy::panic`.
2. **ADRs Obligatoires** :
   - Toute décision architecturale LTP/KISS doit être consignée sous `adr/` et indexée dans [adr/README.md](adr/README.md).

---

## ⚡ Commandes Critiques de Vérification

- `cargo fmt --all -- --check`
- `cargo clippy --offline --workspace --lib --bins -- -D warnings -D clippy::unwrap_used -D clippy::panic`
- `cargo test --offline --all-features`
- `./scripts/check-security.sh --strict`
- `./scripts/check-coverage.sh --summary-only`
