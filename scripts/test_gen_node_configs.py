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


if __name__ == "__main__":
    unittest.main()
