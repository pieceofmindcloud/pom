/**
 * O perfil de quem está olhando a tela.
 *
 * A interface é a mesma em toda máquina do cluster, mas quem abre não é: numa
 * máquina que só empresta camadas, telas como "Implantar na rede" descrevem um
 * trabalho que não é daquela máquina fazer. O perfil é o que separa os dois
 * papéis - `admin` opera o cluster inteiro, `node` cuida da própria máquina e
 * `inference` só conversa com os modelos disponíveis e administra sua API key.
 *
 * **Isto não é autorização.** É apresentação: esconde e bloqueia telas no
 * navegador, e nada mais. A credencial que o nó exige continua sendo a chave de
 * conexão (`api.ts`), verificada no gateway; um perfil trocado aqui não abre
 * nem fecha nenhuma rota da API. Quando o perfil passar a valer de verdade,
 * quem decide é o nó, e esta tela passa a refletir o que ele responder - não o
 * contrário. A autorização do Inference fica para a tarefa
 * `rdma-perfis-escopo-backend`.
 *
 * Hoje o perfil é escolhido à mão pelo seletor da barra superior, que existe
 * só para desenvolvimento: ver `PROFILE_SWITCHER_ENABLED`.
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { storageKey } from "./brand";

export const PROFILES = ["admin", "node", "inference"] as const;

export type Profile = (typeof PROFILES)[number];

/**
 * Enquanto o perfil não vem do nó, o operador precisa de uma forma de ver a
 * tela pelos olhos do outro perfil. Este é o interruptor desse seletor.
 *
 * É uma constante, e não uma configuração: o seletor é andaime de
 * desenvolvimento, e a intenção é **apagá-lo** - junto com esta constante e com
 * `ProfileSwitcher` - assim que o perfil passar a chegar do nó. Vire para
 * `false` para conferir como a barra fica sem ele.
 */
export const PROFILE_SWITCHER_ENABLED = true;

/**
 * Quem abre a tela sem ter escolhido nada é tratado como `admin`.
 *
 * O contrário - abrir como `node` - esconderia telas de quem sempre as teve,
 * e a pessoa não teria como saber que existe um perfil para trocar. Enquanto o
 * perfil é escolha local e não credencial, o padrão permissivo é o honesto.
 */
export const DEFAULT_PROFILE: Profile = "admin";

const STORAGE_KEY = storageKey("profile");

export function isProfile(value: string | null | undefined): value is Profile {
  return PROFILES.includes(value as Profile);
}

function storedProfile(): Profile {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY);
    return isProfile(saved) ? saved : DEFAULT_PROFILE;
  } catch {
    // Navegador com armazenamento bloqueado ainda abre a tela.
    return DEFAULT_PROFILE;
  }
}

type ProfileContextValue = {
  profile: Profile;
  setProfile: (profile: Profile) => void;
};

const ProfileContext = createContext<ProfileContextValue>({
  profile: DEFAULT_PROFILE,
  setProfile: () => {},
});

export function ProfileProvider({
  children,
  initialProfile,
}: {
  children: ReactNode;
  /** Fixa o perfil sem tocar no `localStorage` - usado pelos testes. */
  initialProfile?: Profile;
}) {
  const [profile, setProfileState] = useState<Profile>(
    () => initialProfile ?? storedProfile(),
  );

  useEffect(() => {
    document.documentElement.dataset.profile = profile;
  }, [profile]);

  const setProfile = useCallback((next: Profile) => {
    setProfileState(next);
    try {
      window.localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // A escolha vale nesta aba mesmo sem conseguir persistir.
    }
  }, []);

  const value = useMemo(() => ({ profile, setProfile }), [profile, setProfile]);

  return <ProfileContext.Provider value={value}>{children}</ProfileContext.Provider>;
}

export const useProfile = () => useContext(ProfileContext);
