# Security policy

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: open the Security tab of this
repository and choose "Report a vulnerability". Please do not open a public
issue. Include your Windows version, what you did and what happened.

## In scope

- The app leaving desktop icons hidden after it exits, is paused or crashes.
- The app acting on a file the user did not choose.
- The app loading code from outside System32.
- Any data leaving the machine (the app has no network code).

## Out of scope

- Anything that requires an attacker who already runs code as the same user.
- Third-party shell extensions (icon and context-menu handlers).

## Supported versions

Early release: only the latest release is supported.
