# Sticky Roles Discord Bot

Restores members' roles when they rejoin the server.

## Usage

1. [Invite the bot](https://discord.com/oauth2/authorize?client_id=1557739341017522176) to your server with Manage Roles permission (plus View Audit Log and Manage Nicknames if you use those options). Enable the privileged Server Members (GUILD_MEMBERS) intent for the bot in the Discord developer portal.
2. Run `/sticky-roles` (needs Manage Roles and Manage Guild) to pick tracked roles, a log channel, options (skip bans/kicks, include bots, save nicknames), and how long saved data lasts.
3. Ensure the bot's top role is above any tracked roles, or it can't give them back.
4. When a member who left rejoins, their saved roles are restored automatically and a message is sent to the log channel. Use `/data save` to export a CSV backup and `/data restore` to replace saved data from one.

## Getting Started (dev)

```sh
$ cargo build && DISCORD_TOKEN=your_token ./target/debug/sticky-roles
```

Data is stored in `./data` by default (`DATA_DIR` overrides it).

## Deployment (production)

```sh
$ cargo build --release && DISCORD_TOKEN=your_token ./target/release/sticky-roles

# or docker
$ docker compose build && DISCORD_TOKEN=your_token docker compose up
```
