from typing import Tuple
import psycopg2
import psutil
import os
import signal
import time

SHUTDOWN_TIMEOUT = 5

PGCAT_HOST = "127.0.0.1"
PGCAT_PORT = "6432"


def connect_db(
    autocommit: bool = True,
    admin: bool = False,
) -> Tuple[psycopg2.extensions.connection, psycopg2.extensions.cursor]:

    if admin:
        user = "admin_user"
        password = "admin_pass"
        db = "pgcat"
    else:
        user = "sharding_user"
        password = "sharding_user"
        db = "sharded_db"

    conn = psycopg2.connect(
        f"postgres://{user}:{password}@{PGCAT_HOST}:{PGCAT_PORT}/{db}?application_name=testing_pgcat",
        connect_timeout=2,
    )
    conn.autocommit = autocommit
    cur = conn.cursor()

    return (conn, cur)


def cleanup_conn(conn: psycopg2.extensions.connection, cur: psycopg2.extensions.cursor):
    cur.close()
    conn.close()


def test_normal_db_access():
    conn, cur = connect_db(autocommit=False)
    cur.execute("SELECT 1")
    res = cur.fetchall()
    print(res)
    cleanup_conn(conn, cur)


def test_admin_db_access():
    conn, cur = connect_db(admin=True)

    cur.execute("SHOW POOLS")
    res = cur.fetchall()
    print(res)
    cleanup_conn(conn, cur)

test_normal_db_access()
test_admin_db_access()

