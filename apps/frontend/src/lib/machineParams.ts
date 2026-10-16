/**
 * The configuration one machine of a deployment version runs with.
 *
 * Shared by the deployment history and the dashboard, so the two screens show the same values for the same machine.
 */
import type { DeploymentNode, DeploymentVersion, RuntimeParams } from "./types";

/** The configuration a machine runs with: the one it was launched with, else its own parameters over the version's. */
export function machineParamsOf(node: DeploymentNode, version: DeploymentVersion): RuntimeParams {
  if (node.config && Object.keys(node.config).length > 0) return node.config;
  return version.overrides?.[node.node_id]?.params ?? version.params;
}
