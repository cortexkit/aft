## Saved folder access on the permissions pages

The desktop and phone permissions pages now list the folders you let an agent
or its worker use from AFT's folder-access cards, and you can revoke each one
there. AFT serves the fleet's `permissions/v1` capability through Cingulate.
After a revoke, the agent's next call into that folder asks again. A shell
command that is still running with the folder in its sandbox stops, and the
agent sees it as "killed (folder access revoked)". Commands whose sandbox
never included the folder keep running.

Each saved grant now records when it was granted and which card granted it
(aft.db schema 27). Grants saved before this version show the upgrade time
as their grant time and name no card.
