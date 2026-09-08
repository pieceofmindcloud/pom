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
};

const ProfileContext = createContext<ProfileContextValue>({
  profile: DEFAULT_PROFILE,
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
    <ProfileContextProvider profile={node.data?.profile ?? DEFAULT_PROFILE}>
      {children}
    </ProfileContextProvider>
  );
}

function ProfileContextProvider({
  children,
  profile,
}: {
  children: ReactNode;
  profile: Profile;
}) {

  useEffect(() => {
    document.documentElement.dataset.profile = profile;
  }, [profile]);

  const value = useMemo(() => ({ profile }), [profile]);

  return <ProfileContext.Provider value={value}>{children}</ProfileContext.Provider>;
}

export const useProfile = () => useContext(ProfileContext);
