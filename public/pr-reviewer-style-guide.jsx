import { useState } from "react";

const tokens = {
  colors: {
    cream:      "#E8E0D0",
    creamLight: "#F2EDE4",
    warmGray:   "#C4B9A8",
    teal:       "#3A6B7E",
    tealDark:   "#2C5264",
    tealDeep:   "#1E3A47",
    red:        "#B84233",
    redLight:   "#C65847",
    redMuted:   "#9E3A2D",
    brown:      "#3D2B1F",
    brownLight: "#5C4033",
    brownDark:  "#2A1D14",
    ink:        "#1A1410",
    paper:      "#EDE6D8",
  },
  shadow: {
    raised: "0 1px 0 rgba(255,255,255,0.1) inset, 0 1px 3px rgba(26,20,16,0.12), 0 1px 1px rgba(26,20,16,0.08)",
    raisedHover: "0 1px 0 rgba(255,255,255,0.12) inset, 0 2px 6px rgba(26,20,16,0.14), 0 1px 2px rgba(26,20,16,0.1)",
    pressed: "inset 0 1px 2px rgba(26,20,16,0.12), 0 0 0 rgba(26,20,16,0)",
    inset: "inset 0 1px 3px rgba(26,20,16,0.1), inset 0 0 1px rgba(26,20,16,0.06)",
    insetDeep: "inset 0 2px 4px rgba(26,20,16,0.16), inset 0 0 1px rgba(26,20,16,0.08)",
    panel: "0 1px 0 rgba(255,255,255,0.06) inset, 0 1px 4px rgba(26,20,16,0.1), 0 0 1px rgba(26,20,16,0.06)",
    panelDark: "0 1px 0 rgba(255,255,255,0.04) inset, 0 1px 4px rgba(0,0,0,0.2)",
  }
};

const noiseFilter = `url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='300' height='300'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.85' numOctaves='4' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)' opacity='0.08'/%3E%3C/svg%3E")`;

function Section({ title, subtitle, children, dark }) {
  return (
    <section style={{
      background: dark ? tokens.colors.brownDark : "transparent",
      color: dark ? tokens.colors.cream : tokens.colors.brown,
      padding: "56px 0",
      borderTop: dark ? "none" : `1px solid ${tokens.colors.warmGray}`,
      position: "relative",
    }}>
      {dark && <div style={{
        position: "absolute", inset: 0, pointerEvents: "none",
        boxShadow: "inset 0 1px 6px rgba(0,0,0,0.2)",
      }}/>}
      <div style={{ maxWidth: 960, margin: "0 auto", padding: "0 32px", position: "relative" }}>
        <div style={{ marginBottom: 40 }}>
          <p style={{
            fontFamily: "'JetBrains Mono', monospace",
            fontSize: 11, letterSpacing: "0.15em", textTransform: "uppercase",
            color: dark ? tokens.colors.warmGray : tokens.colors.teal,
            marginBottom: 8,
          }}>{subtitle}</p>
          <h2 style={{
            fontFamily: "'Instrument Serif', Georgia, serif",
            fontSize: 36, fontWeight: 400, lineHeight: 1.15,
            color: dark ? tokens.colors.cream : tokens.colors.brownDark,
            margin: 0,
          }}>{title}</h2>
        </div>
        {children}
      </div>
    </section>
  );
}

function TactileCard({ children, style: s, dark, inset }) {
  return (
    <div style={{
      background: dark ? "#352A20" : tokens.colors.creamLight,
      border: dark ? "1px solid rgba(255,255,255,0.05)" : `1px solid ${tokens.colors.warmGray}`,
      borderTopColor: dark ? "rgba(255,255,255,0.08)" : "#d4ccbc",
      borderBottomColor: dark ? "rgba(0,0,0,0.15)" : "#b8ad9c",
      borderRadius: 10,
      boxShadow: inset ? tokens.shadow.inset : dark ? tokens.shadow.panelDark : tokens.shadow.panel,
      position: "relative",
      overflow: "hidden",
      ...s,
    }}>
      {children}
    </div>
  );
}

function Swatch({ name, hex, role }) {
  const [copied, setCopied] = useState(false);
  const [pressed, setPressed] = useState(false);
  const isLight = ["cream", "creamLight", "warmGray", "paper"].includes(name);
  return (
    <div
      onClick={() => { navigator.clipboard?.writeText(hex); setCopied(true); setTimeout(() => setCopied(false), 1200); }}
      onMouseDown={() => setPressed(true)}
      onMouseUp={() => setPressed(false)}
      onMouseLeave={() => setPressed(false)}
      style={{
        cursor: "pointer",
        background: hex,
        borderRadius: 8,
        padding: "28px 20px 16px",
        minHeight: 110,
        display: "flex", flexDirection: "column", justifyContent: "flex-end",
        position: "relative",
        border: isLight ? `1px solid ${tokens.colors.warmGray}` : "1px solid rgba(255,255,255,0.06)",
        borderTopColor: isLight ? "#d4ccbc" : "rgba(255,255,255,0.1)",
        borderBottomColor: isLight ? "#b8ad9c" : "rgba(0,0,0,0.1)",
        boxShadow: pressed ? tokens.shadow.pressed : tokens.shadow.raised,
        transition: "all 0.08s ease",
        transform: pressed ? "translateY(0.5px)" : "translateY(0)",
      }}
    >
      {copied && <span style={{
        position: "absolute", top: 10, right: 12,
        fontFamily: "'JetBrains Mono', monospace", fontSize: 10,
        color: isLight ? tokens.colors.brown : tokens.colors.cream, opacity: 0.7,
      }}>copied</span>}
      <p style={{
        fontFamily: "'JetBrains Mono', monospace", fontSize: 11, fontWeight: 600,
        color: isLight ? tokens.colors.brown : tokens.colors.cream,
        margin: "0 0 2px", textTransform: "lowercase",
      }}>{name}</p>
      <p style={{
        fontFamily: "'JetBrains Mono', monospace", fontSize: 10,
        color: isLight ? tokens.colors.brownLight : "rgba(255,255,255,0.55)",
        margin: "0 0 4px",
      }}>{hex}</p>
      <p style={{
        fontFamily: "'JetBrains Mono', monospace", fontSize: 9,
        color: isLight ? tokens.colors.teal : "rgba(255,255,255,0.4)",
        margin: 0, letterSpacing: "0.04em",
      }}>{role}</p>
    </div>
  );
}

function TactileButton({ label, variant = "primary" }) {
  const [pressed, setPressed] = useState(false);
  const styles = {
    primary: {
      bg: `linear-gradient(180deg, #4a7d90 0%, ${tokens.colors.teal} 40%, ${tokens.colors.tealDark} 100%)`,
      color: tokens.colors.cream,
      border: `1px solid ${tokens.colors.tealDeep}`,
      borderTop: `1px solid #4a7d90`,
      shadow: "0 1px 0 rgba(255,255,255,0.08) inset, 0 1px 3px rgba(26,20,16,0.14), 0 0 1px rgba(26,20,16,0.1)",
      pressedShadow: "inset 0 1px 3px rgba(0,0,0,0.18)",
    },
    secondary: {
      bg: `linear-gradient(180deg, #f6f1e8 0%, ${tokens.colors.cream} 100%)`,
      color: tokens.colors.teal,
      border: `1px solid ${tokens.colors.warmGray}`,
      borderTop: "1px solid #d8d0c2",
      shadow: "0 1px 0 rgba(255,255,255,0.25) inset, 0 1px 3px rgba(26,20,16,0.08)",
      pressedShadow: "inset 0 1px 2px rgba(26,20,16,0.1)",
    },
    ghost: {
      bg: "transparent", color: tokens.colors.brownLight,
      border: "1px solid transparent", borderTop: "1px solid transparent",
      shadow: "none", pressedShadow: "none",
    },
    danger: {
      bg: `linear-gradient(180deg, #d06050 0%, ${tokens.colors.red} 40%, ${tokens.colors.redMuted} 100%)`,
      color: tokens.colors.cream,
      border: `1px solid ${tokens.colors.redMuted}`,
      borderTop: "1px solid #d06050",
      shadow: "0 1px 0 rgba(255,255,255,0.08) inset, 0 1px 3px rgba(26,20,16,0.14), 0 0 1px rgba(26,20,16,0.1)",
      pressedShadow: "inset 0 1px 3px rgba(0,0,0,0.18)",
    },
  };
  const s = styles[variant];
  return (
    <button
      onMouseDown={() => setPressed(true)}
      onMouseUp={() => setPressed(false)}
      onMouseLeave={() => setPressed(false)}
      style={{
        fontFamily: "'DM Sans', sans-serif", fontSize: 13, fontWeight: 600,
        color: s.color, background: s.bg,
        border: s.border, borderTop: s.borderTop,
        borderRadius: 7, padding: variant === "ghost" ? "8px 4px" : "10px 20px",
        cursor: "pointer",
        textDecoration: variant === "ghost" ? "underline" : "none",
        textUnderlineOffset: "3px",
        boxShadow: pressed ? s.pressedShadow : s.shadow,
        transform: pressed ? "translateY(0.5px)" : "translateY(0)",
        transition: "all 0.08s ease",
        letterSpacing: "0.01em",
      }}
    >{label}</button>
  );
}

function DemoCard({ title, description, children }) {
  return (
    <TactileCard style={{ marginBottom: 24 }}>
      <div style={{
        padding: "14px 24px",
        borderBottom: `1px solid ${tokens.colors.warmGray}`,
        display: "flex", justifyContent: "space-between", alignItems: "baseline",
        background: "linear-gradient(180deg, rgba(255,255,255,0.06) 0%, transparent 100%)",
      }}>
        <h4 style={{
          fontFamily: "'JetBrains Mono', monospace", fontSize: 13, fontWeight: 600,
          color: tokens.colors.brownDark, margin: 0,
        }}>{title}</h4>
        {description && <span style={{
          fontFamily: "'JetBrains Mono', monospace", fontSize: 10,
          color: tokens.colors.teal, letterSpacing: "0.05em",
        }}>{description}</span>}
      </div>
      <div style={{ padding: 24 }}>{children}</div>
    </TactileCard>
  );
}

function SpecNote({ children }) {
  return (
    <div style={{
      marginTop: 16, padding: "12px 16px",
      background: tokens.colors.paper, borderRadius: 6,
      boxShadow: tokens.shadow.inset,
      border: "1px solid rgba(0,0,0,0.03)",
    }}>
      <p style={{
        fontFamily: "'JetBrains Mono', monospace", fontSize: 10,
        color: tokens.colors.brownLight, margin: 0, lineHeight: 1.8,
      }}>{children}</p>
    </div>
  );
}

export default function PRReviewerStyleGuide() {
  const [activeTab, setActiveTab] = useState("overview");
  const [pressedTab, setPressedTab] = useState(null);
  const tabs = [
    { id: "overview", label: "Overview" },
    { id: "color", label: "Color" },
    { id: "typography", label: "Type" },
    { id: "components", label: "Components" },
    { id: "patterns", label: "Patterns" },
    { id: "voice", label: "Voice" },
  ];

  return (
    <div style={{
      fontFamily: "'Charter', 'Georgia', serif",
      color: tokens.colors.brown,
      background: tokens.colors.paper,
      minHeight: "100vh",
      backgroundImage: noiseFilter,
    }}>
      <link href="https://fonts.googleapis.com/css2?family=Instrument+Serif:ital@0;1&family=JetBrains+Mono:wght@400;500;600;700&family=DM+Sans:wght@400;500;600;700&display=swap" rel="stylesheet" />

      {/* ── HEADER ── */}
      <header style={{
        background: `linear-gradient(180deg, #352A20 0%, ${tokens.colors.brownDark} 100%)`,
        backgroundImage: noiseFilter,
        padding: "48px 32px 56px",
        position: "relative", overflow: "hidden",
        borderBottom: `1px solid rgba(255,255,255,0.04)`,
        boxShadow: "0 1px 4px rgba(0,0,0,0.2)",
      }}>
        <div style={{ display: "flex", gap: 7, marginBottom: 32 }}>
          {[tokens.colors.red, tokens.colors.redLight, tokens.colors.warmGray, tokens.colors.teal, tokens.colors.tealDark].map((c, i) => (
            <div key={i} style={{
              width: 10, height: 10, borderRadius: "50%",
              background: c,
              borderTop: "1px solid rgba(255,255,255,0.15)",
              borderBottom: "1px solid rgba(0,0,0,0.2)",
              boxShadow: "0 1px 2px rgba(0,0,0,0.25)",
            }} />
          ))}
        </div>
        <div style={{ maxWidth: 960, margin: "0 auto" }}>
          <div style={{ display: "flex", alignItems: "flex-end", gap: 24, marginBottom: 16 }}>
            <h1 style={{
              fontFamily: "'DM Sans', sans-serif", fontSize: 52, fontWeight: 700,
              color: tokens.colors.cream, margin: 0, lineHeight: 1, letterSpacing: "-0.02em",
            }}>pr-reviewer</h1>
            <span style={{
              fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
              color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase",
              paddingBottom: 8,
            }}>style reference v1.0</span>
          </div>
          <p style={{
            fontFamily: "'Instrument Serif', Georgia, serif", fontSize: 22,
            color: tokens.colors.warmGray, margin: "0 0 8px", fontStyle: "italic",
            maxWidth: 520, lineHeight: 1.4,
          }}>
            Component & style guide for an open-source code review tool that runs at home and doesn't phone home.
          </p>
        </div>
      </header>

      {/* ── NAV ── */}
      <nav style={{
        background: `linear-gradient(180deg, ${tokens.colors.cream} 0%, #DDD6C6 100%)`,
        borderBottom: `1px solid #B8AD9C`,
        borderTop: `1px solid rgba(255,255,255,0.4)`,
        position: "sticky", top: 0, zIndex: 100,
        boxShadow: "0 1px 3px rgba(26,20,16,0.08)",
      }}>
        <div style={{
          maxWidth: 960, margin: "0 auto", padding: "6px 32px",
          display: "flex", gap: 4,
        }}>
          {tabs.map(t => {
            const isActive = activeTab === t.id;
            const isPressed = pressedTab === t.id;
            return (
              <button
                key={t.id}
                onClick={() => setActiveTab(t.id)}
                onMouseDown={() => setPressedTab(t.id)}
                onMouseUp={() => setPressedTab(null)}
                onMouseLeave={() => setPressedTab(null)}
                style={{
                  fontFamily: "'JetBrains Mono', monospace", fontSize: 12,
                  fontWeight: isActive ? 600 : 400,
                  color: isActive ? tokens.colors.brownDark : tokens.colors.brownLight,
                  background: isActive
                    ? tokens.colors.creamLight
                    : "transparent",
                  border: isActive ? `1px solid ${tokens.colors.warmGray}` : "1px solid transparent",
                  borderBottom: isActive ? `1px solid ${tokens.colors.creamLight}` : "1px solid transparent",
                  borderTopColor: isActive ? "#d4ccbc" : "transparent",
                  borderRadius: "7px 7px 0 0",
                  padding: "10px 18px 8px",
                  cursor: "pointer",
                  letterSpacing: "0.03em",
                  transition: "all 0.08s ease",
                  position: "relative",
                  marginBottom: -1,
                  boxShadow: isActive
                    ? "inset 0 1px 0 rgba(255,255,255,0.3)"
                    : isPressed
                    ? "inset 0 1px 2px rgba(26,20,16,0.08)"
                    : "none",
                  transform: isPressed && !isActive ? "translateY(0.5px)" : "none",
                }}
              >
                {isActive && <div style={{
                  position: "absolute", top: 0, left: 1, right: 1, height: 2,
                  background: tokens.colors.red, borderRadius: "2px 2px 0 0",
                }}/>}
                {t.label}
              </button>
            );
          })}
        </div>
      </nav>

      {/* ── OVERVIEW ── */}
      {activeTab === "overview" && (
        <>
          <Section title="Design Philosophy" subtitle="01 / Foundation">
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 20 }}>
              {[
                { head: "Retro-Analog Infrastructure", body: "Inspired by CRT monitors, tape decks, and broadcast equipment. The aesthetic says: this tool has been running reliably since before your SaaS provider existed. Distressed textures, warm paper tones, and hardware-inspired UI elements." },
                { head: "Earned Minimalism", body: "Not minimal because we're lazy — minimal because every element was interrogated. If it's here, it's load-bearing. Dense where density serves clarity (CLI output, config), spacious where breathing room helps (docs, onboarding)." },
                { head: "Trust Through Transparency", body: "The design language reflects the product philosophy: nothing hidden, nothing clever for cleverness's sake. Monospace for anything the tool actually outputs. Serif for anything humans write. Clear hierarchy, no decoration for decoration." },
                { head: "Quietly Confident", body: "No gradients screaming 'innovation.' No purple hero sections. The palette is warm and grounded — teal for trust, red for action, cream for comfort, brown for craft. It looks like something you'd find in a well-organized workshop." },
              ].map((item, i) => (
                <TactileCard key={i} style={{ padding: "28px 24px" }}>
                  <h3 style={{
                    fontFamily: "'DM Sans', sans-serif", fontSize: 15, fontWeight: 700,
                    color: tokens.colors.brownDark, margin: "0 0 12px",
                  }}>{item.head}</h3>
                  <p style={{
                    fontFamily: "'DM Sans', sans-serif", fontSize: 14, lineHeight: 1.6,
                    color: tokens.colors.brownLight, margin: 0,
                  }}>{item.body}</p>
                </TactileCard>
              ))}
            </div>
          </Section>

          <Section title="Logo Usage" subtitle="02 / Identity" dark>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 20 }}>
              {[
                { bg: tokens.colors.paper, label: "On Light", labelColor: tokens.colors.brown },
                { bg: tokens.colors.brownDark, label: "On Dark", labelColor: tokens.colors.warmGray },
                { bg: tokens.colors.tealDeep, label: "On Brand", labelColor: tokens.colors.cream },
              ].map((v, i) => (
                <TactileCard key={i} dark style={{ padding: 32, display: "flex", flexDirection: "column", alignItems: "center", gap: 16 }}>
                  <div style={{
                    background: v.bg, borderRadius: 10, padding: 24,
                    boxShadow: tokens.shadow.inset,
                    border: "1px solid rgba(0,0,0,0.08)",
                    width: "100%", display: "flex", justifyContent: "center",
                  }}>
                    <div style={{
                      width: 72, height: 72, borderRadius: "50%",
                      background: `linear-gradient(135deg, ${tokens.colors.red}, ${tokens.colors.teal})`,
                      display: "flex", alignItems: "center", justifyContent: "center",
                      borderTop: "1px solid rgba(255,255,255,0.12)",
                      borderBottom: "1px solid rgba(0,0,0,0.15)",
                      boxShadow: "0 1px 4px rgba(0,0,0,0.2)",
                    }}>
                      <span style={{
                        fontFamily: "'DM Sans', sans-serif", fontSize: 20, fontWeight: 700,
                        color: tokens.colors.cream,
                      }}>PR</span>
                    </div>
                  </div>
                  <span style={{
                    fontFamily: "'JetBrains Mono', monospace", fontSize: 10,
                    color: v.labelColor, letterSpacing: "0.1em", textTransform: "uppercase",
                  }}>{v.label}</span>
                </TactileCard>
              ))}
            </div>
            <div style={{
              marginTop: 24, padding: "16px 20px",
              background: "rgba(0,0,0,0.15)", borderRadius: 8,
              boxShadow: "inset 0 1px 3px rgba(0,0,0,0.15)",
              border: "1px solid rgba(255,255,255,0.03)",
            }}>
              <p style={{
                fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
                color: tokens.colors.warmGray, margin: 0, lineHeight: 1.7,
              }}>
                Minimum clear space: 1× logo width on all sides. Never place on busy photo backgrounds without a backing shape. The emblem (badge mark) can be used standalone at 32px+ for favicons, GitHub avatars, and small UI placements.
              </p>
            </div>
          </Section>
        </>
      )}

      {/* ── COLOR ── */}
      {activeTab === "color" && (
        <>
          <Section title="Color Palette" subtitle="01 / Palette">
            <p style={{
              fontFamily: "'DM Sans', sans-serif", fontSize: 14, lineHeight: 1.7,
              color: tokens.colors.brownLight, maxWidth: 600, marginBottom: 32,
            }}>
              Derived from the logo's retro broadcast palette. Warm, grounded tones that feel like aged paper and hardware patina. Click any swatch to copy its hex value.
            </p>

            <p style={{
              fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
              color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase",
              marginBottom: 12,
            }}>Primary</p>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 14, marginBottom: 32 }}>
              <Swatch name="teal" hex="#3A6B7E" role="Primary action / links" />
              <Swatch name="red" hex="#B84233" role="Accent / CTA / alerts" />
              <Swatch name="brown" hex="#3D2B1F" role="Text / headings" />
              <Swatch name="cream" hex="#E8E0D0" role="Background / surface" />
            </div>

            <p style={{
              fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
              color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase",
              marginBottom: 12,
            }}>Extended</p>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(5, 1fr)", gap: 14, marginBottom: 32 }}>
              <Swatch name="tealDark" hex="#2C5264" role="Hover / pressed" />
              <Swatch name="tealDeep" hex="#1E3A47" role="Dark surfaces" />
              <Swatch name="redLight" hex="#C65847" role="Hover accent" />
              <Swatch name="redMuted" hex="#9E3A2D" role="Pressed accent" />
              <Swatch name="brownDark" hex="#2A1D14" role="Dark bg / headers" />
            </div>

            <p style={{
              fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
              color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase",
              marginBottom: 12,
            }}>Neutrals</p>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 14 }}>
              <Swatch name="paper" hex="#EDE6D8" role="Page background" />
              <Swatch name="creamLight" hex="#F2EDE4" role="Card / elevated" />
              <Swatch name="warmGray" hex="#C4B9A8" role="Borders / muted" />
              <Swatch name="ink" hex="#1A1410" role="Maximum contrast" />
            </div>
          </Section>

          <Section title="Color Application" subtitle="02 / Usage" dark>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 24 }}>
              <div style={{
                background: tokens.colors.paper, borderRadius: 10, padding: 24,
                boxShadow: tokens.shadow.inset,
                border: "1px solid rgba(0,0,0,0.04)",
              }}>
                <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase", margin: "0 0 16px" }}>Light Mode</p>
                <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
                  {[
                    { color: tokens.colors.paper, label: "page-bg: paper", border: true },
                    { color: tokens.colors.creamLight, label: "surface: cream-light", border: true },
                    { color: tokens.colors.brown, label: "text: brown" },
                    { color: tokens.colors.teal, label: "interactive: teal" },
                  ].map((row, j) => (
                    <div key={j} style={{ display: "flex", alignItems: "center", gap: 12 }}>
                      <div style={{
                        width: 28, height: 28, borderRadius: 6,
                        background: row.color,
                        border: row.border ? `1px solid ${tokens.colors.warmGray}` : "1px solid rgba(0,0,0,0.08)",
                        borderTopColor: row.border ? "#d4ccbc" : "rgba(255,255,255,0.1)",
                        boxShadow: "0 1px 2px rgba(0,0,0,0.08)",
                      }} />
                      <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, color: tokens.colors.brown }}>{row.label}</span>
                    </div>
                  ))}
                </div>
              </div>
              <div style={{
                background: tokens.colors.ink, borderRadius: 10, padding: 24,
                boxShadow: tokens.shadow.insetDeep,
                border: "1px solid rgba(255,255,255,0.04)",
              }}>
                <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase", margin: "0 0 16px" }}>Dark Mode</p>
                <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
                  {[
                    { color: tokens.colors.brownDark, label: "page-bg: brown-dark" },
                    { color: "#352A20", label: "surface: brown + 8%" },
                    { color: tokens.colors.cream, label: "text: cream" },
                    { color: tokens.colors.redLight, label: "interactive: red-light" },
                  ].map((row, j) => (
                    <div key={j} style={{ display: "flex", alignItems: "center", gap: 12 }}>
                      <div style={{
                        width: 28, height: 28, borderRadius: 6,
                        background: row.color,
                        border: "1px solid rgba(255,255,255,0.08)",
                        boxShadow: "0 1px 2px rgba(0,0,0,0.15)",
                      }} />
                      <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, color: tokens.colors.cream }}>{row.label}</span>
                    </div>
                  ))}
                </div>
              </div>
            </div>
          </Section>
        </>
      )}

      {/* ── TYPOGRAPHY ── */}
      {activeTab === "typography" && (
        <>
          <Section title="Type System" subtitle="01 / Typefaces">
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 20 }}>
              {[
                { name: "Instrument Serif", role: "Display / Editorial", sample: "Thoughtful code review.", family: "'Instrument Serif', Georgia, serif", weight: 400, size: 32, style: "italic", note: "Long-form headings, hero text, editorial moments. The human voice of the brand." },
                { name: "DM Sans", role: "UI / Body", sample: "Your reviews, your infra.", family: "'DM Sans', sans-serif", weight: 600, size: 28, note: "Interface text, navigation, body copy, buttons. The workhorse — clear at every size." },
                { name: "JetBrains Mono", role: "Code / Data / System", sample: "--dry-run first", family: "'JetBrains Mono', monospace", weight: 500, size: 22, note: "CLI output, code, labels, technical details. Anything the tool actually says." },
              ].map((tf, i) => (
                <TactileCard key={i} style={{ padding: "32px 24px 24px", display: "flex", flexDirection: "column" }}>
                  <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.12em", textTransform: "uppercase", margin: "0 0 20px" }}>{tf.role}</p>
                  <p style={{ fontFamily: tf.family, fontWeight: tf.weight, fontSize: tf.size, fontStyle: tf.style || "normal", color: tokens.colors.brownDark, margin: "0 0 20px", lineHeight: 1.2 }}>{tf.sample}</p>
                  <div style={{ marginTop: "auto" }}>
                    <p style={{ fontFamily: "'DM Sans', sans-serif", fontSize: 14, fontWeight: 700, color: tokens.colors.brown, margin: "0 0 6px" }}>{tf.name}</p>
                    <p style={{ fontFamily: "'DM Sans', sans-serif", fontSize: 12, color: tokens.colors.brownLight, margin: 0, lineHeight: 1.5 }}>{tf.note}</p>
                  </div>
                </TactileCard>
              ))}
            </div>
          </Section>

          <Section title="Type Scale" subtitle="02 / Hierarchy">
            <TactileCard style={{ padding: 0, overflow: "hidden" }}>
              {[
                { label: "display-lg", family: "'Instrument Serif', serif", size: 48, weight: 400, lh: 1.1, style: "italic", sample: "Code review that runs at home" },
                { label: "display-sm", family: "'Instrument Serif', serif", size: 32, weight: 400, lh: 1.2, sample: "Thoughtful defaults, zero lock-in" },
                { label: "heading-lg", family: "'DM Sans', sans-serif", size: 24, weight: 700, lh: 1.25, sample: "Configuration" },
                { label: "heading-sm", family: "'DM Sans', sans-serif", size: 18, weight: 600, lh: 1.3, sample: "Getting Started" },
                { label: "body", family: "'DM Sans', sans-serif", size: 15, weight: 400, lh: 1.65, sample: "pr-reviewer watches your repos, picks up new PRs, and posts a review. That's it." },
                { label: "label", family: "'JetBrains Mono', monospace", size: 11, weight: 600, lh: 1.4, sample: "OPEN SOURCE CODE REVIEW TOOL", transform: "uppercase", ls: "0.12em" },
                { label: "code", family: "'JetBrains Mono', monospace", size: 13, weight: 400, lh: 1.5, sample: "pr-reviewer --repo org/app --dry-run" },
                { label: "caption", family: "'JetBrains Mono', monospace", size: 10, weight: 400, lh: 1.5, sample: "Last reviewed: 2 minutes ago · 3 files changed", ls: "0.02em" },
              ].map((row, i) => (
                <div key={i} style={{
                  display: "grid", gridTemplateColumns: "120px 1fr", gap: 24,
                  padding: "16px 24px",
                  borderBottom: i < 7 ? `1px solid ${tokens.colors.warmGray}40` : "none",
                  alignItems: "baseline",
                }}>
                  <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.05em" }}>{row.label}</span>
                  <span style={{ fontFamily: row.family, fontSize: row.size, fontWeight: row.weight, fontStyle: row.style || "normal", lineHeight: row.lh, color: tokens.colors.brownDark, textTransform: row.transform || "none", letterSpacing: row.ls || "normal" }}>{row.sample}</span>
                </div>
              ))}
            </TactileCard>
          </Section>
        </>
      )}

      {/* ── COMPONENTS ── */}
      {activeTab === "components" && (
        <>
          <Section title="UI Components" subtitle="01 / Interactive Elements">
            <DemoCard title="Buttons" description="primary / secondary / ghost / danger">
              <div style={{ display: "flex", gap: 14, flexWrap: "wrap", alignItems: "center" }}>
                <TactileButton label="Start Review" variant="primary" />
                <TactileButton label="Configure" variant="secondary" />
                <TactileButton label="View Logs" variant="ghost" />
                <TactileButton label="Revoke Token" variant="danger" />
              </div>
              <SpecNote>
                Three-stop gradient (highlight → base → shadow). Top border 1px lighter, bottom border 1px darker than face. Drop shadow: 1px blur, 12% opacity. Pressed: inset 1px shadow, translateY(0.5px). Transition: 80ms ease.
              </SpecNote>
            </DemoCard>

            <DemoCard title="Status Badges" description="review states">
              <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
                {[
                  { label: "reviewing", bg: `${tokens.colors.teal}14`, color: tokens.colors.teal, dot: tokens.colors.teal },
                  { label: "approved", bg: "#2D6A4F14", color: "#2D6A4F", dot: "#2D6A4F" },
                  { label: "changes requested", bg: `${tokens.colors.red}14`, color: tokens.colors.red, dot: tokens.colors.red },
                  { label: "queued", bg: `${tokens.colors.warmGray}30`, color: tokens.colors.brownLight, dot: tokens.colors.warmGray },
                  { label: "error", bg: `${tokens.colors.red}0c`, color: tokens.colors.redMuted, dot: tokens.colors.redMuted },
                ].map((b, i) => (
                  <span key={i} style={{
                    display: "inline-flex", alignItems: "center", gap: 6,
                    fontFamily: "'JetBrains Mono', monospace", fontSize: 11, fontWeight: 500,
                    color: b.color, background: b.bg,
                    padding: "5px 12px 5px 10px", borderRadius: 999,
                    border: "1px solid rgba(0,0,0,0.04)",
                    borderTopColor: "rgba(255,255,255,0.06)",
                  }}>
                    <span style={{
                      width: 6, height: 6, borderRadius: "50%", flexShrink: 0,
                      background: b.dot,
                      boxShadow: `0 0 0 1px ${b.dot}30`,
                    }} />
                    {b.label}
                  </span>
                ))}
              </div>
            </DemoCard>

            <DemoCard title="CLI Output Block" description="terminal aesthetic">
              <div style={{
                background: tokens.colors.ink,
                borderRadius: 10, padding: "20px 24px",
                fontFamily: "'JetBrains Mono', monospace", fontSize: 12, lineHeight: 1.7,
                boxShadow: "inset 0 1px 4px rgba(0,0,0,0.25)",
                border: "1px solid rgba(0,0,0,0.2)",
                borderTopColor: "rgba(0,0,0,0.3)",
                borderBottomColor: "rgba(255,255,255,0.02)",
                position: "relative", overflow: "hidden",
              }}>
                {/* Faint scanlines */}
                <div style={{
                  position: "absolute", inset: 0, pointerEvents: "none", opacity: 0.025,
                  background: "repeating-linear-gradient(0deg, transparent, transparent 2px, rgba(255,255,255,0.4) 2px, rgba(255,255,255,0.4) 4px)",
                }}/>
                <div style={{ position: "relative" }}>
                  <div style={{ color: tokens.colors.warmGray, marginBottom: 4 }}>
                    <span style={{ color: tokens.colors.teal }}>$</span> pr-reviewer --repo acme/api --dry-run
                  </div>
                  <div style={{ color: tokens.colors.warmGray, opacity: 0.35, marginBottom: 8 }}>
                    ──────────────────────────────────────────
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    <span style={{ color: tokens.colors.teal }}>●</span> watching <span style={{ fontWeight: 600 }}>acme/api</span>
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    <span style={{ color: "#2D6A4F" }}>✓</span> found 2 open PRs
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    <span style={{ color: tokens.colors.teal }}>→</span> PR #847 <span style={{ color: tokens.colors.warmGray }}>fix: race condition in claim lock</span>
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    {"  "}<span style={{ color: tokens.colors.warmGray }}>3 files changed</span> · <span style={{ color: "#2D6A4F" }}>+42</span> <span style={{ color: tokens.colors.red }}>-18</span>
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    {"  "}<span style={{ color: tokens.colors.redLight }}>⚑</span> <span style={{ color: tokens.colors.redLight }}>1 concern</span><span style={{ color: tokens.colors.warmGray }}> — missing mutex release in error path</span>
                  </div>
                  <div style={{ color: tokens.colors.cream }}>
                    {"  "}<span style={{ color: "#2D6A4F" }}>✓</span> <span style={{ color: "#5A9A78" }}>2 approvals</span><span style={{ color: tokens.colors.warmGray }}> — idempotency check, ETag handling</span>
                  </div>
                  <div style={{ color: tokens.colors.cream, marginTop: 4 }}>
                    <span style={{ color: tokens.colors.warmGray, opacity: 0.5 }}>[dry-run]</span> would post review to PR #847
                  </div>
                </div>
              </div>
              <SpecNote>
                Inset shadow creates recessed screen feel. Scanlines at 2.5% opacity. Top border slightly darker, bottom border carries faint highlight — the "bezel" effect. No glow — just depth.
              </SpecNote>
            </DemoCard>

            <DemoCard title="Content Card" description="info / feature blocks">
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 16 }}>
                {[
                  { icon: "⚙", title: "Idempotent by Default", desc: "Reviews post once. Restarts, crashes, duplicate events — it handles all of it. Your PR gets one review comment, period." },
                  { icon: "◎", title: "ETag Caching", desc: "Only fetches what changed. Respects rate limits. Doesn't burn your API quota re-reading files it already reviewed." },
                ].map((card, i) => (
                  <TactileCard key={i} style={{ padding: "24px 20px" }}>
                    <div style={{
                      width: 36, height: 36, borderRadius: 8,
                      background: `${tokens.colors.teal}0c`,
                      display: "flex", alignItems: "center", justifyContent: "center",
                      marginBottom: 14, fontSize: 17,
                      boxShadow: tokens.shadow.inset,
                      border: `1px solid ${tokens.colors.teal}15`,
                    }}>{card.icon}</div>
                    <h4 style={{
                      fontFamily: "'DM Sans', sans-serif", fontSize: 14, fontWeight: 700,
                      color: tokens.colors.brownDark, margin: "0 0 8px",
                    }}>{card.title}</h4>
                    <p style={{
                      fontFamily: "'DM Sans', sans-serif", fontSize: 13,
                      color: tokens.colors.brownLight, margin: 0, lineHeight: 1.6,
                    }}>{card.desc}</p>
                  </TactileCard>
                ))}
              </div>
            </DemoCard>

            <DemoCard title="Form Elements" description="inputs / controls">
              <div style={{ display: "flex", flexDirection: "column", gap: 20, maxWidth: 400 }}>
                <div>
                  <label style={{
                    fontFamily: "'JetBrains Mono', monospace", fontSize: 11, fontWeight: 600,
                    color: tokens.colors.brownDark, display: "block", marginBottom: 8,
                  }}>Repository</label>
                  <div style={{
                    borderRadius: 8,
                    boxShadow: tokens.shadow.inset,
                    border: `1px solid ${tokens.colors.warmGray}`,
                    borderTopColor: "#b8ad9c",
                    borderBottomColor: "#d4ccbc",
                    background: tokens.colors.paper,
                  }}>
                    <input type="text" defaultValue="acme/api" style={{
                      fontFamily: "'JetBrains Mono', monospace", fontSize: 13,
                      color: tokens.colors.brownDark, background: "transparent",
                      border: "none", padding: "10px 14px",
                      width: "100%", boxSizing: "border-box", outline: "none",
                    }} />
                  </div>
                  <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.warmGray, margin: "6px 0 0" }}>owner/repo format</p>
                </div>
                {/* Toggle */}
                <div style={{ display: "flex", gap: 14, alignItems: "center" }}>
                  <div style={{
                    width: 40, height: 22, borderRadius: 11,
                    background: tokens.colors.teal, position: "relative", cursor: "pointer",
                    boxShadow: "inset 0 1px 3px rgba(0,0,0,0.2)",
                    border: `1px solid ${tokens.colors.tealDark}`,
                    borderTopColor: tokens.colors.tealDeep,
                    borderBottomColor: "#4a7d90",
                  }}>
                    <div style={{
                      position: "absolute", top: 2, right: 2,
                      width: 16, height: 16, borderRadius: "50%",
                      background: `linear-gradient(180deg, #fff 0%, ${tokens.colors.cream} 100%)`,
                      border: "1px solid rgba(0,0,0,0.08)",
                      borderTopColor: "rgba(255,255,255,0.5)",
                      boxShadow: "0 1px 2px rgba(0,0,0,0.15)",
                    }}/>
                  </div>
                  <span style={{ fontFamily: "'DM Sans', sans-serif", fontSize: 13, color: tokens.colors.brown }}>Enable dry-run mode</span>
                </div>
                {/* Checkbox */}
                <div style={{ display: "flex", gap: 12, alignItems: "center" }}>
                  <div style={{
                    width: 18, height: 18, borderRadius: 4,
                    background: `linear-gradient(180deg, #4a7d90 0%, ${tokens.colors.tealDark} 100%)`,
                    display: "flex", alignItems: "center", justifyContent: "center",
                    border: `1px solid ${tokens.colors.tealDeep}`,
                    borderTopColor: "#4a7d90",
                    boxShadow: "0 1px 2px rgba(26,20,16,0.12)",
                    flexShrink: 0,
                  }}>
                    <span style={{ color: tokens.colors.cream, fontSize: 12, lineHeight: 1 }}>✓</span>
                  </div>
                  <span style={{ fontFamily: "'DM Sans', sans-serif", fontSize: 13, color: tokens.colors.brown }}>Post inline comments</span>
                </div>
              </div>
              <SpecNote>
                Inputs: inset shadow, top border darker than bottom (light falls from above). Toggle knob: 2-stop gradient (white → cream), 1px shadow. Checkbox: same gradient language as primary button. Focus ring: 0 0 0 2px teal/20%.
              </SpecNote>
            </DemoCard>
          </Section>
        </>
      )}

      {/* ── PATTERNS ── */}
      {activeTab === "patterns" && (
        <>
          <Section title="Design Patterns" subtitle="01 / Texture & Depth">
            <DemoCard title="Paper Texture & Noise" description="analog warmth">
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 16 }}>
                {[
                  { bg: tokens.colors.paper, label: "paper + noise 8%", light: true },
                  { bg: tokens.colors.brownDark, label: "dark + noise 8%", light: false },
                  { bg: tokens.colors.tealDeep, label: "teal-deep + noise 8%", light: false },
                ].map((t, i) => (
                  <div key={i} style={{
                    height: 110, borderRadius: 10,
                    background: t.bg, backgroundImage: noiseFilter,
                    display: "flex", alignItems: "center", justifyContent: "center",
                    border: t.light ? `1px solid ${tokens.colors.warmGray}` : "1px solid rgba(255,255,255,0.05)",
                    boxShadow: tokens.shadow.inset,
                  }}>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: t.light ? tokens.colors.brownLight : tokens.colors.warmGray }}>{t.label}</span>
                  </div>
                ))}
              </div>
              <SpecNote>
                Apply fractalNoise (baseFrequency: 0.85, octaves: 4) at 6–10% opacity over large background surfaces. Simulates aged paper and broadcast grain. Never on interactive elements or text.
              </SpecNote>
            </DemoCard>

            <DemoCard title="Surface Depth Model" description="directional light, not glow">
              <div style={{ display: "flex", gap: 24, alignItems: "center", justifyContent: "center", padding: "20px 0" }}>
                {[
                  { name: "recessed", desc: "inputs, wells", shadow: tokens.shadow.insetDeep, bg: tokens.colors.paper, borderT: "#b8ad9c", borderB: "#d4ccbc" },
                  { name: "flush", desc: "badges, labels", shadow: "none", bg: tokens.colors.creamLight, borderT: "#d4ccbc", borderB: "#b8ad9c" },
                  { name: "raised", desc: "cards, panels", shadow: tokens.shadow.panel, bg: tokens.colors.creamLight, borderT: "#d4ccbc", borderB: "#b8ad9c" },
                  { name: "floating", desc: "modals, tooltips", shadow: `${tokens.shadow.panel}, 0 4px 12px rgba(26,20,16,0.1)`, bg: tokens.colors.creamLight, borderT: "#d4ccbc", borderB: "#b8ad9c" },
                ].map((s, i) => (
                  <div key={i} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10 }}>
                    <div style={{
                      width: 80, height: 56, borderRadius: 8,
                      background: s.bg,
                      boxShadow: s.shadow,
                      border: `1px solid ${tokens.colors.warmGray}60`,
                      borderTopColor: s.borderT,
                      borderBottomColor: s.borderB,
                    }} />
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.brownLight, fontWeight: 600 }}>{s.name}</span>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.warmGray }}>{s.desc}</span>
                  </div>
                ))}
              </div>
              <SpecNote>
                Depth comes from directional border color (top lighter, bottom darker — light source is above) and tight, low-opacity shadows. Not from blur radius or glow. The difference between "raised" and "floating" is one extra shadow layer at wider offset, not more blur.
              </SpecNote>
            </DemoCard>

            <DemoCard title="The Edge-Light Trick" description="the core technique">
              <div style={{
                background: tokens.colors.paper, borderRadius: 10, padding: 32,
                display: "flex", flexDirection: "column", alignItems: "center", gap: 24,
                boxShadow: tokens.shadow.inset,
              }}>
                <div style={{ display: "flex", gap: 32, alignItems: "center" }}>
                  {/* Without */}
                  <div style={{ textAlign: "center" }}>
                    <div style={{
                      width: 120, height: 72, borderRadius: 8,
                      background: tokens.colors.creamLight,
                      border: `1px solid ${tokens.colors.warmGray}`,
                      boxShadow: "0 1px 3px rgba(26,20,16,0.1)",
                      marginBottom: 8,
                    }}/>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.warmGray }}>uniform border</span>
                  </div>
                  <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 18, color: tokens.colors.warmGray }}>→</span>
                  {/* With */}
                  <div style={{ textAlign: "center" }}>
                    <div style={{
                      width: 120, height: 72, borderRadius: 8,
                      background: tokens.colors.creamLight,
                      border: `1px solid ${tokens.colors.warmGray}`,
                      borderTopColor: "#d4ccbc",
                      borderBottomColor: "#b8ad9c",
                      boxShadow: "0 1px 0 rgba(255,255,255,0.06) inset, 0 1px 3px rgba(26,20,16,0.1)",
                      marginBottom: 8,
                    }}/>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal }}>directional edges</span>
                  </div>
                </div>
                <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, color: tokens.colors.brownLight, textAlign: "center", maxWidth: 400, lineHeight: 1.7, margin: 0 }}>
                  Top border: 1 step lighter than side borders. Bottom border: 1 step darker. Single inset top highlight at 6% white. This is all you need — the eye reads it as a physical edge catching light.
                </p>
              </div>
            </DemoCard>

            <DemoCard title="Border & Divider Language" description="structural rhythm">
              <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
                {[
                  { border: `1px solid ${tokens.colors.warmGray}`, label: "1px solid warm-gray — section divider", color: tokens.colors.warmGray },
                  { border: `1.5px solid ${tokens.colors.teal}`, label: "1.5px solid teal — active / focus", color: tokens.colors.teal },
                  { border: `2px solid ${tokens.colors.red}`, label: "2px solid red — emphasis / nav active", color: tokens.colors.red },
                  { border: `1px dashed ${tokens.colors.warmGray}80`, label: "1px dashed warm-gray/50 — secondary", color: tokens.colors.warmGray },
                ].map((d, i) => (
                  <div key={i} style={{ display: "flex", alignItems: "center", gap: 16 }}>
                    <div style={{ flex: 1, borderBottom: d.border }} />
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: d.color, whiteSpace: "nowrap" }}>{d.label}</span>
                  </div>
                ))}
              </div>
            </DemoCard>

            <DemoCard title="Spacing & Radius" description="8px grid + corner scale">
              <div style={{ display: "flex", gap: 24 }}>
                <div style={{ flex: 1 }}>
                  <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase", margin: "0 0 12px" }}>Spacing</p>
                  <div style={{ display: "flex", gap: 10, alignItems: "flex-end" }}>
                    {[{ name: "xs", val: 4 }, { name: "sm", val: 8 }, { name: "md", val: 16 }, { name: "lg", val: 24 }, { name: "xl", val: 32 }, { name: "xxl", val: 48 }].map((s, i) => (
                      <div key={i} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 6 }}>
                        <div style={{
                          width: 36, height: s.val,
                          background: `${tokens.colors.teal}18`, borderRadius: 3,
                          boxShadow: tokens.shadow.inset,
                          border: `1px solid ${tokens.colors.teal}20`,
                        }} />
                        <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.brownLight }}>{s.name}</span>
                        <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 8, color: tokens.colors.warmGray }}>{s.val}px</span>
                      </div>
                    ))}
                  </div>
                </div>
                <div style={{ flex: 1 }}>
                  <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase", margin: "0 0 12px" }}>Radius</p>
                  <div style={{ display: "flex", gap: 14, alignItems: "center" }}>
                    {[{ name: "sm", val: 3 }, { name: "md", val: 7 }, { name: "lg", val: 10 }, { name: "full", val: 9999 }].map((r, i) => (
                      <div key={i} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 8 }}>
                        <div style={{
                          width: 44, height: 44,
                          background: `linear-gradient(180deg, #4a7d90 0%, ${tokens.colors.tealDark} 100%)`,
                          borderRadius: r.val,
                          borderTop: "1px solid #5a8d9a",
                          borderBottom: "1px solid #1E3A47",
                          boxShadow: "0 1px 3px rgba(26,20,16,0.12)",
                        }} />
                        <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.brownLight }}>{r.name}</span>
                      </div>
                    ))}
                  </div>
                </div>
              </div>
            </DemoCard>

            <DemoCard title="Shadow Recipes" description="the full set">
              <div style={{ display: "grid", gridTemplateColumns: "repeat(5, 1fr)", gap: 14 }}>
                {[
                  { name: "raised", val: tokens.shadow.raised, desc: "Buttons, swatches" },
                  { name: "panel", val: tokens.shadow.panel, desc: "Cards, sections" },
                  { name: "inset", val: tokens.shadow.inset, desc: "Inputs, wells" },
                  { name: "inset-deep", val: tokens.shadow.insetDeep, desc: "Terminal, screens" },
                  { name: "pressed", val: tokens.shadow.pressed, desc: "Active buttons" },
                ].map((s, i) => (
                  <div key={i} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 8 }}>
                    <div style={{
                      width: "100%", height: 48, borderRadius: 8,
                      background: s.name.includes("inset") || s.name === "pressed" ? tokens.colors.paper : tokens.colors.creamLight,
                      border: `1px solid ${tokens.colors.warmGray}60`,
                      borderTopColor: s.name.includes("inset") || s.name === "pressed" ? "#b8ad9c" : "#d4ccbc",
                      borderBottomColor: s.name.includes("inset") || s.name === "pressed" ? "#d4ccbc" : "#b8ad9c",
                      boxShadow: s.val,
                    }} />
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.brownLight, fontWeight: 600 }}>{s.name}</span>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.warmGray, textAlign: "center" }}>{s.desc}</span>
                  </div>
                ))}
              </div>
            </DemoCard>
          </Section>
        </>
      )}

      {/* ── VOICE ── */}
      {activeTab === "voice" && (
        <Section title="Voice & Tone Reference" subtitle="01 / How It Speaks">
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 20, marginBottom: 28 }}>
            <TactileCard>
              <div style={{
                padding: "14px 20px",
                background: "#2D6A4F08",
                borderBottom: `1px solid ${tokens.colors.warmGray}`,
              }}>
                <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, fontWeight: 600, color: "#2D6A4F" }}>✓ Sounds like this</span>
              </div>
              <div style={{ padding: "20px", display: "flex", flexDirection: "column", gap: 16 }}>
                {[
                  { ctx: "readme", text: "No API keys. Uses the CLI subscription you already have." },
                  { ctx: "tagline", text: "Code review that runs at home and doesn't phone home." },
                  { ctx: "feature", text: "It posts once. Even if you restart it three times." },
                  { ctx: "cli", text: "Dry-run first. Post when you're ready." },
                  { ctx: "error", text: "Couldn't reach GitHub. Check your token has repo scope." },
                ].map((ex, i) => (
                  <div key={i}>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase" }}>{ex.ctx}</span>
                    <p style={{ fontFamily: "'Instrument Serif', Georgia, serif", fontSize: 16, color: tokens.colors.brownDark, margin: "4px 0 0", lineHeight: 1.45, fontStyle: "italic" }}>"{ex.text}"</p>
                  </div>
                ))}
              </div>
            </TactileCard>
            <TactileCard>
              <div style={{
                padding: "14px 20px",
                background: `${tokens.colors.red}06`,
                borderBottom: `1px solid ${tokens.colors.warmGray}`,
              }}>
                <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, fontWeight: 600, color: tokens.colors.red }}>✗ Never sounds like this</span>
              </div>
              <div style={{ padding: "20px", display: "flex", flexDirection: "column", gap: 16 }}>
                {[
                  { ctx: "hype", text: "AI-powered code review at scale!" },
                  { ctx: "growth-speak", text: "Boost developer velocity with intelligent automation." },
                  { ctx: "grandiose", text: "The future of code review is here." },
                  { ctx: "vague error", text: "Something went wrong. Please try again." },
                  { ctx: "blame-y", text: "Invalid input. Make sure you typed it correctly." },
                ].map((ex, i) => (
                  <div key={i}>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 9, color: tokens.colors.red, letterSpacing: "0.1em", textTransform: "uppercase" }}>{ex.ctx}</span>
                    <p style={{ fontFamily: "'DM Sans', sans-serif", fontSize: 15, color: tokens.colors.brownLight, margin: "4px 0 0", lineHeight: 1.45, textDecoration: "line-through", textDecorationColor: `${tokens.colors.red}30` }}>"{ex.text}"</p>
                  </div>
                ))}
              </div>
            </TactileCard>
          </div>

          <DemoCard title="Tone by Context" description="adapt, don't flatten">
            <table style={{ width: "100%", borderCollapse: "collapse", fontFamily: "'DM Sans', sans-serif", fontSize: 13 }}>
              <thead>
                <tr>
                  {["Context", "Tone", "Example"].map((h, i) => (
                    <th key={i} style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, fontWeight: 600, color: tokens.colors.teal, letterSpacing: "0.1em", textTransform: "uppercase", textAlign: "left", padding: "8px 12px 12px", borderBottom: `1.5px solid ${tokens.colors.warmGray}` }}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {[
                  { ctx: "README / docs", tone: "Direct, dry, a little wry", ex: "Explains the why once." },
                  { ctx: "Error messages", tone: "Honest and specific", ex: "Never blames the user implicitly." },
                  { ctx: "CLI output", tone: "Compact and structured", ex: "Uses color and hierarchy, not walls of text." },
                  { ctx: "Taglines", tone: "Punchy, earns its brevity", ex: "No filler words." },
                  { ctx: "GitHub issues", tone: "Warm and collaborative", ex: "Drops the terseness." },
                ].map((row, i) => (
                  <tr key={i}>
                    <td style={{ padding: "10px 12px", borderBottom: `1px solid ${tokens.colors.warmGray}40`, fontWeight: 600, color: tokens.colors.brownDark }}>{row.ctx}</td>
                    <td style={{ padding: "10px 12px", borderBottom: `1px solid ${tokens.colors.warmGray}40`, color: tokens.colors.brown }}>{row.tone}</td>
                    <td style={{ padding: "10px 12px", borderBottom: `1px solid ${tokens.colors.warmGray}40`, color: tokens.colors.brownLight, fontStyle: "italic" }}>{row.ex}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </DemoCard>

          <DemoCard title="Personality Axes" description="where we sit">
            <div style={{ display: "flex", flexDirection: "column", gap: 24 }}>
              {[
                { left: "Arrogant", right: "Obsequious", pos: 35, label: "Opinionated, not arrogant" },
                { left: "Cold", right: "Chatty", pos: 40, label: "Warm without being chatty" },
                { left: "Loud", right: "Invisible", pos: 65, label: "Quietly confident" },
                { left: "Clever", right: "Plain", pos: 45, label: "Craft-focused" },
              ].map((axis, i) => (
                <div key={i}>
                  <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 8 }}>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.warmGray }}>{axis.left}</span>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.warmGray }}>{axis.right}</span>
                  </div>
                  <div style={{
                    height: 6, borderRadius: 3,
                    background: tokens.colors.paper,
                    boxShadow: "inset 0 1px 2px rgba(26,20,16,0.1)",
                    border: "1px solid rgba(0,0,0,0.04)",
                    borderTopColor: "rgba(0,0,0,0.07)",
                    position: "relative",
                  }}>
                    <div style={{
                      position: "absolute", top: 0, bottom: 0, left: 0,
                      width: `${axis.pos}%`, borderRadius: 3,
                      background: `${tokens.colors.teal}25`,
                    }}/>
                    <div style={{
                      position: "absolute", left: `${axis.pos}%`, top: "50%",
                      transform: "translate(-50%, -50%)",
                      width: 14, height: 14, borderRadius: "50%",
                      background: `linear-gradient(180deg, #fff 0%, ${tokens.colors.cream} 100%)`,
                      border: `1px solid ${tokens.colors.warmGray}`,
                      borderTopColor: "#d4ccbc",
                      borderBottomColor: "#b8ad9c",
                      boxShadow: "0 1px 2px rgba(26,20,16,0.12)",
                    }} />
                  </div>
                  <p style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 10, color: tokens.colors.teal, margin: "8px 0 0", textAlign: "center" }}>{axis.label}</p>
                </div>
              ))}
            </div>
          </DemoCard>
        </Section>
      )}

      <footer style={{
        background: `linear-gradient(180deg, ${tokens.colors.brownDark} 0%, ${tokens.colors.ink} 100%)`,
        padding: "32px", textAlign: "center",
        borderTop: `2px solid ${tokens.colors.red}`,
        boxShadow: "inset 0 1px 4px rgba(0,0,0,0.2)",
      }}>
        <p style={{
          fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
          color: tokens.colors.warmGray, margin: 0,
        }}>
          pr-reviewer · style reference v1.0 · open source code review tool
        </p>
      </footer>
    </div>
  );
}
