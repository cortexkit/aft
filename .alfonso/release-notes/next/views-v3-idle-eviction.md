With index views enabled, storage retention now reclaims idle v3 checkout views
when their family's allocated disk usage exceeds its 2 GiB target, oldest first.
Views with live readers, pins, sessions, or publication work are kept. Reopening
an evicted checkout rebuilds its view. Views remain off by default.
