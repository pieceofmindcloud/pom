/**
 * O perfil oficial que o nó entrega à interface.
 *
 * A configuração persistida no próprio nó é a fonte de verdade. O contexto
 * continua sendo usado pelas rotas e pelo menu para manter uma única política
 * de apresentação, mas não oferece setter nem lê localStorage.
 */
import { useContext, useEffect, useMemo, createContext, type ReactNode } from "react";
import { useNode } from "./queries";

export const PROFILES = ["admin", "node", "infer"] as const;

export type Profile = (typeof PROFILES)[number];

/** Fallback transitório enquanto a tela de setup ainda não expõe `/api/ui/node`. */
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
};

const ProfileContext = createContext<ProfileContextValue>({
  profile: DEFAULT_PROFILE,
  secondary: false,
});

export function ProfileProvider({
  children,
  initialProfile,
}: {
  children: ReactNode;
  /** Fixa o perfil somente nos testes de apresentação. */
  initialProfile?: Profile;
}) {
  if (initialProfile !== undefined) {
    return <ProfileContextProvider profile={initialProfile}>{children}</ProfileContextProvider>;
  }

  return <ProfileFromNode>{children}</ProfileFromNode>;
}

function ProfileFromNode({ children }: { children: ReactNode }) {
  const node = useNode();
  return (
    <ProfileContextProvider
      profile={node.data?.profile ?? DEFAULT_PROFILE}
      secondary={node.data?.authority_level === "secondary"}
    >
      {children}
    </ProfileContextProvider>
  );
}

function ProfileContextProvider({
  children,
  profile,
  secondary = false,
}: {
  children: ReactNode;
  profile: Profile;
  secondary?: boolean;
}) {

  useEffect(() => {
    document.documentElement.dataset.profile = profile;
  }, [profile]);

  const value = useMemo(() => ({ profile, secondary }), [profile, secondary]);

  return <ProfileContext.Provider value={value}>{children}</ProfileContext.Provider>;
}

export const useProfile = () => useContext(ProfileContext);
