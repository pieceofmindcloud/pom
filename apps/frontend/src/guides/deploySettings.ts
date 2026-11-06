/**
 * The guides behind the help button of each category of the deployment
 * inspector: Default settings and POM settings. The Beta group and its
 * `beta-settings` guide are off while it has no setting (see
 * `components/DeploymentInspector.tsx`).
 *
 * Their text lives in `docs/guides/settings/`; this module only names their
 * `?guide=` ids for the inspector. Same rule as the other guides: every number
 * and behaviour there is in the field hints (`deploy.knob.*Hint`) or in
 * `docs/subsystems/deployment.md`.
 */

/** The `?guide=` id of each inspector category's guide. */
export const DEPLOY_SETTINGS_GUIDES = {
  defaults: "default-settings",
  pom: "pom-settings",
} as const;
