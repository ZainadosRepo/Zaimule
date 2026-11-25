# Auto-héberger votre propre relais multijoueur Zaimule (`zaimule-relay`)

Ce dossier contient le serveur relais WebSocket Rust autonome (Axum + Tokio, ~15 Mo RAM) permettant à n'importe qui disposant de son propre VPS, serveur Linux, NAS ou Raspberry Pi d'héberger un relais **100 % privé** pour jouer avec ses amis sur **Zaimule** (GB/GBC, Switch `Torzu`/`Fritchii`, et prochainement GBA/NDS).

## Modèle de sécurité

- **Votre clé privée (`ZAIMULE_RELAY_ACCESS_KEY`)** : personne sans cette clé ne peut ouvrir de connexion WebSocket (`401 Unauthorized` avant même l'upgrade WebSocket).
- **Chiffrement de bout en bout (`ZMR1`, `ChaCha20-Poly1305`)** : le serveur ne voit jamais les entrées, paquets Switch LDN ou messages de chat en clair.
- **0 port ouvert sur les PC / box des joueurs** : toutes les connexions Zaimule sont sortantes en `wss://` (TLS 1.3 sur le port `443`).

## Déploiement rapide en 3 étapes (VPS avec Docker)

### 1. Générer votre clé privée (ne jamais la publier)

```bash
cd server/zaimule-relay
umask 077
printf 'ZAIMULE_RELAY_ACCESS_KEY=%s\n' "$(openssl rand -hex 32)" > .env
chmod 600 .env
cat .env
```

Conservez la valeur générée après `=` : vous la donnerez uniquement aux amis autorisés à utiliser votre relais.

### 2. Lancer le relais

- **Option A — Vous avez déjà un reverse proxy (Caddy / Nginx / Traefik)** :
  ```bash
  docker compose up -d --build
  ```
  Le relais écoute uniquement sur `127.0.0.1:3000`. Ajoutez une route reverse proxy HTTPS vers `127.0.0.1:3000` (sans compression HTTP, `flush_interval -1` sous Caddy ou `proxy_buffering off` sous Nginx).

- **Option B — Serveur vierge avec TLS automatique (Caddy inclus)** :
  Ajoutez dans `.env` :
  ```bash
  RELAY_DOMAIN=relay.votre-domaine.fr
  ACME_EMAIL=vous@votre-domaine.fr
  ```
  *(Astuce sans nom de domaine : vous pouvez utiliser `relay.<votre-ip-avec-tirets>.sslip.io`.)*

  Puis démarrez :
  ```bash
  docker compose --profile tls up -d --build
  ```

### 3. Configurer Zaimule (chez vous et vos amis)

1. Ouvrez **Zaimule → Paramètres → Multijoueur E2EE**.
2. **Adresse du relais VPS (WSS)** : `wss://relay.votre-domaine.fr/ws`
3. **Clé d'accès privée** : collez la clé générée à l'étape 1 puis cliquez sur **Enregistrer**.
4. Créez un salon et partagez le lien `zaimule://room/CODE?...#CLE` à vos amis !
