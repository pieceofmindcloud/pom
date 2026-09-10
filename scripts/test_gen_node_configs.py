import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("gen-node-configs.py")
SPEC = importlib.util.spec_from_file_location("gen_node_configs", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class AdvertiseAddressTests(unittest.TestCase):
    def test_override_is_used_when_generating_a_new_node_config(self):
        base = {
            "connection_key": "shared",
            "advertise_addr": "old-ipv6",
            "model_dir": "/models",
        }

        config = MODULE.config_do_no(
            base,
            n=2,
            total=5,
            advertise_addr="new-ipv6",
        )

        self.assertEqual(config["advertise_addr"], "new-ipv6")

    def test_override_updates_only_the_advertise_address_of_existing_config(self):
        existing = {
            "connection_key": "shared",
            "advertise_addr": "old-ipv6",
            "node_name": "ksia-2",
            "custom_setting": "keep-me",
        }

        updated = MODULE.config_com_advertise_addr(existing, "new-ipv6")

        self.assertEqual(updated["advertise_addr"], "new-ipv6")
        self.assertEqual(updated["custom_setting"], "keep-me")
        self.assertEqual(existing["advertise_addr"], "old-ipv6")

    def test_explicit_host_port_override_keeps_each_nodes_p2p_port(self):
        base = {
            "connection_key": "shared",
            "advertise_addr": "old-ipv6",
        }

        node_two = MODULE.config_do_no(
            base,
            n=2,
            total=4,
            advertise_addr="100.122.132.97:42777",
        )

        self.assertEqual(node_two["p2p_port"], 42778)
        self.assertEqual(node_two["advertise_addr"], "100.122.132.97:42778")

    def test_existing_config_override_uses_its_own_p2p_port(self):
        existing = {
            "connection_key": "shared",
            "advertise_addr": "old-ipv6",
            "p2p_port": 42780,
        }

        updated = MODULE.config_com_advertise_addr(
            existing,
            "100.122.132.97:42777",
        )

        self.assertEqual(updated["advertise_addr"], "100.122.132.97:42780")


if __name__ == "__main__":
    unittest.main()
