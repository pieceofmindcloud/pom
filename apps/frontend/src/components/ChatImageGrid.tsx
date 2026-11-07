// What a Chat answer generated: images as a grid of thumbnails read from this
// node by hash (one opens full size with Download), video and audio with a
// player and Download.
import { Download } from "lucide-react";
import { useEffect, useState } from "react";
import { useI18n } from "../i18n/provider";
import { loadChatImage } from "../lib/chatImage";
import type { ChatImage } from "../lib/types";
import { Modal } from "./Confirm";
import { Button } from "./ui";

function useImageUrl(hash: string): { url: string | null; missing: boolean } {
  const [state, setState] = useState<{ url: string | null; missing: boolean }>({ url: null, missing: false });
  useEffect(() => {
    let alive = true;
    let made: string | null = null;
    loadChatImage(hash)
      .then((url) => {
        made = url;
        if (alive) setState({ url, missing: url == null });
      })
      .catch(() => alive && setState({ url: null, missing: true }));
    return () => {
      alive = false;
      if (made) URL.revokeObjectURL?.(made);
    };
  }, [hash]);
  return state;
}

function Thumbnail({ image, onOpen }: { image: ChatImage; onOpen: (url: string) => void }) {
  const { t } = useI18n();
  const { url, missing } = useImageUrl(image.hash);
  if (missing) {
    return <div className="flex h-40 w-40 items-center justify-center rounded-lg border border-line bg-raised p-3 text-center text-[0.72rem] text-muted">{t("chatImage.missing")}</div>;
  }
  if (!url) return <div className="h-40 w-40 animate-pulse rounded-lg bg-raised" />;
  return (
    <button
      type="button"
      onClick={() => onOpen(url)}
      title={t("chatImage.open")}
      className="h-40 w-40 overflow-hidden rounded-lg border border-line transition hover:border-iris/60 hover:shadow-panel"
    >
      <img src={url} alt={t("chatImage.alt", { width: image.width, height: image.height })} className="h-full w-full object-cover" />
    </button>
  );
}

function Player({ media }: { media: ChatImage }) {
  const { t } = useI18n();
  const { url, missing } = useImageUrl(media.hash);
  if (missing) {
    return <div className="rounded-lg border border-line bg-raised p-3 text-[0.72rem] text-muted">{t("chatImage.missing")}</div>;
  }
  if (!url) return <div className={media.kind === "video" ? "h-40 w-64 animate-pulse rounded-lg bg-raised" : "h-10 w-72 animate-pulse rounded-lg bg-raised"} />;
  return (
    <div className="flex max-w-full flex-col gap-1">
      {media.kind === "video"
        ? <video src={url} controls playsInline className="max-h-80 max-w-full rounded-lg border border-line" />
        : <audio src={url} controls className="w-72 max-w-full" />}
      <a href={url} download={`${media.hash}.${media.ext}`} className="inline-flex items-center gap-1 text-[0.72rem] text-muted hover:text-fg">
        <Download className="h-3 w-3" />{t("chatImage.download")}
      </a>
    </div>
  );
}

export function ChatImageGrid({ images }: { images: ChatImage[] }) {
  const { t } = useI18n();
  const [open, setOpen] = useState<{ url: string; image: ChatImage } | null>(null);
  const pictures = images.filter((item) => (item.kind ?? "image") === "image");
  const players = images.filter((item) => item.kind === "video" || item.kind === "audio");
  return (
    <>
      {pictures.length > 0 && (
        <div className="flex flex-wrap gap-2" data-testid="chat-images">
          {pictures.map((image) => <Thumbnail key={image.hash} image={image} onOpen={(url) => setOpen({ url, image })} />)}
        </div>
      )}
      {players.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-3" data-testid="chat-media">
          {players.map((media) => <Player key={media.hash} media={media} />)}
        </div>
      )}
      <Modal
        open={open != null}
        title={t("chatImage.title")}
        size="xl"
        onClose={() => setOpen(null)}
        footer={open ? (
          <a href={open.url} download={`${open.image.hash}.${open.image.ext}`}>
            <Button variant="outline"><Download className="h-3.5 w-3.5" />{t("chatImage.download")}</Button>
          </a>
        ) : null}
      >
        {open && <img src={open.url} alt={t("chatImage.alt", { width: open.image.width, height: open.image.height })} className="mx-auto max-h-[75vh]" />}
      </Modal>
    </>
  );
}
