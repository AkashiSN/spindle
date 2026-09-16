export function Placeholder({ title, note }: { title: string; note: string }) {
  return (
    <section className="placeholder">
      <h1>{title}</h1>
      <p className="muted">{note}</p>
    </section>
  )
}
