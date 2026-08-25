import os
import time

import dcmnorm

FIXTURES = os.path.join(os.path.dirname(__file__), "..", "..", "..", "test", "files")
FIXTURE = os.path.join(FIXTURES, "us.dcm")


def check(condition, message="assertion failed"):
    if not condition:
        raise AssertionError(message)


def main():
    import tempfile

    logs = []
    tmp_dir = tempfile.mkdtemp(prefix="dcmnorm-python-dimse-")

    def on_find(filter_dict):
        return [
            {
                "StudyInstanceUID": "1.2.3",
                "PatientName": "FIND^TEST",
                "PatientID": "MRN1",
            }
        ]

    def on_move(study_instance_uid, move_destination_ae):
        return True

    def on_association_complete(stored_by_study):
        logs.append(("assoc_complete", stored_by_study))

    server = dcmnorm.start_dicom_server(
        0,
        tmp_dir,
        "PYTEST-SCP",
        on_find,
        on_move,
        on_association_complete,
        on_log=lambda message: logs.append(("scp_log", message)),
    )
    check(server.local_port > 0, "server should bind an ephemeral port")

    destination = f"127.0.0.1:{server.local_port}"

    status = dcmnorm.echo_scu(destination, on_log=lambda message: logs.append(("echo_log", message)))
    check(status == 0, f"echo_scu should succeed, got status {status}")
    check(any(kind == "echo_log" for kind, _ in logs), "on_log should have been called for echo_scu")

    results = dcmnorm.store_scu(destination, [FIXTURE])
    check(len(results) == 1, "store_scu should report one result")
    check(results[0].status == 0, f"store_scu should succeed, got status {results[0].status}")
    check(len(results[0].sop_instance_uid) > 0)

    # Give the server's association-complete callback a moment to fire (its own connection
    # thread, asynchronous relative to store_scu's own association).
    time.sleep(0.5)
    check(any(kind == "assoc_complete" for kind, _ in logs), "on_association_complete should have fired after C-STORE")

    matches = dcmnorm.find_scu(destination, {"StudyInstanceUID": ""})
    check(len(matches) == 1, "find_scu should report one match from on_find")

    move_handle = dcmnorm.move_scu(destination, "SOME-AE", "1.2.3")
    move_result = move_handle.result()
    check(move_result.status is not None, "move_scu should report a terminal status")

    # start_echo_scu / abort() handle path.
    handle = dcmnorm.start_echo_scu(destination)
    aborted_status = handle.result()
    check(aborted_status == 0, "start_echo_scu handle should resolve like echo_scu")

    server.close()
    server.close()  # idempotent

    print("dimse smoke test passed")


if __name__ == "__main__":
    main()
