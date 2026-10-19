/**
 * The official profile the node hands to the interface.
 *
 * The configuration persisted on the node itself is the source of truth. The context
 * is still used by the routes and the menu to keep a single presentation
 * policy, but it offers no setter and does not read localStorage.
 */
import { useContext, useEffect, useMemo, createContext, type ReactNode } from "react";
import { useNode } from "./queries";

export const PROFILES = ["admin", "node", "infer"] as const;

export type Profile = (typeof PROFILES)[number];

/** Transitional fallback while the setup screen does not yet expose `/api/ui/node`. */
export const DEFAULT_PROFILE: Profile = "admin";

export function isProfile(value: string | null | undefined): value is Profile {
  return PROFILES.includes(value as Profile);
}

type ProfileContextValue = {
  profile: Profile;
  /**
   * An `admin` that is not the network's principal. A secondary admin
   * deploys and undeploys models and otherwise sees what a node sees:
   * members, admissions, settings and the trust chain are the principal's.
   */
  secondary: boolean;
  /**
   * Whether the node has answered with its profile. Until then `profile` is only
   * the `admin` fallback, and nothing may be offered or opened on its strength:
   * an inference node saw the administrator's menu and screens (administrative
   * preferences included) while `/api/ui/node` was pending or failing.
   */
  known: boolean;
};

const ProfileContext = createContext<ProfileContextValue>({
  profile: DEFAULT_PROFILE,
  secondary: false,
  known: false,
});

export function ProfileProvider({
  children,
  initialProfile,
}: {
  children: ReactNode;
  /** Pins the profile only in presentation tests. */
  initialProfile?: Profile;
}) {
  if (initialProfile !== undefined) {
    return <ProfileContextProvider profile={initialProfile} known>{children}</ProfileContextProvider>;
  }

  return <ProfileFromNode>{children}</ProfileFromNode>;
}

function ProfileFromNode({ children }: { children: ReactNode }) {
  const node = useNode();
  return (
    <ProfileContextProvider
      profile={node.data?.profile ?? DEFAULT_PROFILE}
      secondary={node.data?.authority_level === "secondary"}
      known={isProfile(node.data?.profile)}
    >
      {children}
    </ProfileContextProvider>
  );
}

function ProfileContextProvider({
  children,
  profile,
  secondary = false,
  known,
}: {
  children: ReactNode;
  profile: Profile;
  secondary?: boolean;
  known: boolean;
}) {

  useEffect(() => {
    document.documentElement.dataset.profile = profile;
  }, [profile]);

  const value = useMemo(() => ({ profile, secondary, known }), [profile, secondary, known]);

  return <ProfileContext.Provider value={value}>{children}</ProfileContext.Provider>;
}

export const useProfile = () => useContext(ProfileContext);
