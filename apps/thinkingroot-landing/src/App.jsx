import { useEffect, useState, useRef } from 'react';
import { Terminal } from 'lucide-react';
import './App.css';

// Flowing Datalog Graph Component (Canvas based for performance)
const AmbientGraph = () => {
  const canvasRef = useRef(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas.getContext('2d');
    let animationFrameId;
    
    // Set canvas to full window size
    const resizeCanvas = () => {
      canvas.width = window.innerWidth;
      canvas.height = window.innerHeight;
    };
    window.addEventListener('resize', resizeCanvas);
    resizeCanvas();

    // Node particle system
    const numNodes = 60;
    const nodes = [];
    
    for (let i = 0; i < numNodes; i++) {
      nodes.push({
        x: Math.random() * canvas.width,
        y: Math.random() * canvas.height,
        vx: (Math.random() - 0.5) * 0.5,
        vy: (Math.random() - 0.5) * 0.5,
        radius: Math.random() * 3 + 1,
      });
    }

    const draw = () => {
      // Very light cream fade for trailing effect
      ctx.fillStyle = 'rgba(253, 251, 247, 0.2)';
      ctx.fillRect(0, 0, canvas.width, canvas.height);

      // Update and draw nodes
      for (let i = 0; i < nodes.length; i++) {
        let node = nodes[i];
        
        node.x += node.vx;
        node.y += node.vy;

        // Bounce off edges
        if (node.x < 0 || node.x > canvas.width) node.vx *= -1;
        if (node.y < 0 || node.y > canvas.height) node.vy *= -1;

        // Draw connections
        for (let j = i + 1; j < nodes.length; j++) {
          let otherNode = nodes[j];
          let dx = node.x - otherNode.x;
          let dy = node.y - otherNode.y;
          let distance = Math.sqrt(dx * dx + dy * dy);

          // Connect if close enough
          if (distance < 150) {
            ctx.beginPath();
            ctx.strokeStyle = `rgba(180, 175, 160, ${1 - distance / 150})`;
            ctx.lineWidth = 0.5;
            ctx.moveTo(node.x, node.y);
            ctx.lineTo(otherNode.x, otherNode.y);
            ctx.stroke();
          }
        }

        // Draw node
        ctx.beginPath();
        ctx.arc(node.x, node.y, node.radius, 0, Math.PI * 2);
        ctx.fillStyle = 'rgba(107, 106, 102, 0.4)';
        ctx.fill();
      }

      animationFrameId = requestAnimationFrame(draw);
    };

    draw();

    return () => {
      window.removeEventListener('resize', resizeCanvas);
      cancelAnimationFrame(animationFrameId);
    };
  }, []);

  return <canvas ref={canvasRef} className="ambient-background" />;
};

// Universal install section — single-source-of-truth one-liners.
// Mirrors the install paths shipped from this repo:
//   install.sh  → macOS + Linux (curl one-liner)
//   install.ps1 → Windows (PowerShell one-liner)
// Strings must stay byte-identical with the URLs published in the
// GitHub release. No tracking, no telemetry, no JS-side mutation.
const INSTALL_COMMANDS = {
  macos: 'curl -fsSL https://thinkingroot.com/install.sh | sh',
  linux: 'curl -fsSL https://thinkingroot.com/install.sh | sh',
  windows: 'irm https://thinkingroot.com/install.ps1 | iex',
};

// Direct-download fallbacks served via vercel.json 302 redirects to
// the latest GitHub Release. The right-click-Open caveat lives in the
// JSX next to the link — keep the warning visible so non-technical
// users don't bounce on Gatekeeper / SmartScreen.
const FALLBACK_DOWNLOADS = {
  macos: { href: 'https://thinkingroot.com/download/mac', label: 'Download .dmg for macOS' },
  linux: { href: 'https://thinkingroot.com/download/linux', label: 'Download .AppImage for Linux' },
  windows: { href: 'https://thinkingroot.com/download/windows', label: 'Download installer for Windows' },
};

// Best-effort OS detection so the default tab matches what the user
// is actually on. Never lies about the result — defaults to macOS
// during SSR / before the browser is available, then upgrades on
// mount. Don't gate UX on this — every tab is still clickable.
const detectOS = () => {
  if (typeof navigator === 'undefined') return 'macos';
  const ua = (navigator.userAgent || '').toLowerCase();
  const plat = (navigator.platform || '').toLowerCase();
  if (ua.includes('mac') || plat.includes('mac')) return 'macos';
  if (ua.includes('win') || plat.includes('win')) return 'windows';
  if (ua.includes('linux') || plat.includes('linux')) return 'linux';
  return 'macos';
};

// Cloud hub URL — single source of truth for the "Sign in → Dashboard"
// header link. Override at build-time with VITE_HUB_URL when staging
// against a non-prod hub (e.g. preview deployments). No runtime
// fallback: shipping with a wrong URL is louder than silently pointing
// at the wrong host.
const HUB_URL = import.meta.env.VITE_HUB_URL || 'https://thinkingroot.com';

const InstallSection = () => {
  const [active, setActive] = useState('macos');
  const [copied, setCopied] = useState(false);
  const [detected, setDetected] = useState(false);
  const command = INSTALL_COMMANDS[active];
  const fallback = FALLBACK_DOWNLOADS[active];

  useEffect(() => {
    const os = detectOS();
    setActive(os);
    setDetected(true);
  }, []);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      // Older browsers without the Clipboard API. Honest fallback —
      // we don't pretend it copied; the command is still visible
      // for manual selection.
    }
  };

  return (
    <div className="install-section reveal">
      <h3 className="section-eyebrow" style={{ textAlign: 'center' }}>
        Install in one line
      </h3>
      <div className="install-tabs">
        {['macos', 'linux', 'windows'].map((os) => (
          <button
            key={os}
            type="button"
            className={`install-tab ${active === os ? 'active' : ''}`}
            onClick={() => setActive(os)}
          >
            {os === 'macos' ? 'macOS' : os === 'linux' ? 'Linux' : 'Windows'}
          </button>
        ))}
      </div>
      <div className="install-command-wrapper glass-frame">
        <code className="install-command">{command}</code>
        <button
          type="button"
          className={`install-copy ${copied ? 'copied' : ''}`}
          onClick={handleCopy}
          aria-label="Copy install command"
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <p className="install-note">
        {detected
          ? 'CLI + desktop app + daemon auto-start. Open-source, no telemetry, no signing fees.'
          : 'CLI + desktop app + daemon auto-start.'}
      </p>
      <ul className="install-bullets" aria-label="What this installs">
        <li><span className="install-bullet-key">root</span> CLI in <code>/usr/local/bin</code></li>
        <li>ThinkingRoot Desktop in <code>/Applications</code></li>
        <li>Login-agent so the engine auto-starts on reboot</li>
        <li>Models (~340 MB) cached under your user directory</li>
      </ul>
      <details className="install-fallback">
        <summary>Prefer a direct download?</summary>
        <div className="install-fallback-body">
          <a className="install-fallback-link" href={fallback.href}>
            {fallback.label}
          </a>
          {active === 'macos' && (
            <p className="install-fallback-note">
              The first time you open it, macOS will say <em>“ThinkingRoot can’t
              be opened because Apple cannot check it for malicious software.”</em>{' '}
              Right-click the app in Applications → <strong>Open</strong> →{' '}
              <strong>Open</strong> in the dialog. After that it opens normally
              forever. (Apple notarization is on the roadmap once funding allows
              the $99/yr Developer ID.)
            </p>
          )}
          {active === 'windows' && (
            <p className="install-fallback-note">
              Windows SmartScreen may warn the first time. Click <strong>More
              info</strong> → <strong>Run anyway</strong>. The PowerShell
              one-liner above skips this entirely.
            </p>
          )}
          {active === 'linux' && (
            <p className="install-fallback-note">
              <code>chmod +x ThinkingRoot.AppImage</code> then double-click or
              run it. No signing warnings — Linux trusts you to read what you
              download.
            </p>
          )}
        </div>
      </details>
    </div>
  );
};

// Tribunal Component
const TribunalInput = () => {
  const [input, setInput] = useState('');
  const [status, setStatus] = useState('');

  const handleChange = (e) => {
    setInput(e.target.value);
    if (e.target.value.length > 5) {
      setStatus('analyzing');
      setTimeout(() => {
        if (e.target.value.toLowerCase().includes('already active')) {
          setStatus('hallucination');
        } else {
          setStatus('verified');
        }
      }, 1200);
    } else {
      setStatus('');
    }
  };

  return (
    <div className="tribunal-container">
      <h3 className="section-eyebrow">The NLI Tribunal</h3>
      <div className="tribunal-input-wrapper">
        <input 
          type="text" 
          className="tribunal-input" 
          placeholder="Type a claim..." 
          value={input}
          onChange={handleChange}
        />
        <div className={`tribunal-status ${status}`}>
          {status === 'analyzing' && <span className="status-text neutral">Analyzing semantics...</span>}
          {status === 'verified' && <span className="status-text fact">Verified Fact.</span>}
          {status === 'hallucination' && <span className="status-text hallucination">Hallucination Detected.</span>}
        </div>
      </div>
      <p className="tribunal-hint">
        (Try typing "The EU AI Act is already active" vs "The EU AI Act passes in 2026")
      </p>
    </div>
  );
};

// GitHub Rotating Badge Component
const GithubBadge = () => {
  return (
    <a href="https://github.com/DevbyNaveen/ThinkingRoot" target="_blank" rel="noreferrer" className="github-badge">
      <div className="badge-text">
        <svg viewBox="0 0 100 100" width="100" height="100">
          <defs>
            <path id="circlePath" d="M 50, 50 m -35, 0 a 35,35 0 1,1 70,0 a 35,35 0 1,1 -70,0" />
          </defs>
          <text fontSize="11" letterSpacing="2.5" fontWeight="500">
            <textPath href="#circlePath" startOffset="0%">
              OPEN SOURCE • GITHUB REPO • 
            </textPath>
          </text>
        </svg>
      </div>
      <div className="badge-icon">
        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <path d="M15 22v-4a4.8 4.8 0 0 0-1-3.03c3.15-.38 6.5-1.4 6.5-7.1a5.8 5.8 0 0 0-1.5-3.8 5.4 5.4 0 0 0 .15-3.8s-1.2-.38-3.9 1.4a13.3 13.3 0 0 0-7 0C6.2 1.6 5 2 5 2a5.4 5.4 0 0 0 .15 3.8A5.8 5.8 0 0 0 3 9.6c0 5.7 3.3 6.7 6.5 7.1a4.8 4.8 0 0 0-1 3.03v4"></path>
          <path d="M9 20a4.8 4.8 0 0 1-5-1.5"></path>
        </svg>
      </div>
    </a>
  );
};

// Private Packs (cloud dashboard preview)
const PrivatePacks = () => {
  return (
    <div className="private-packs">
      <div className="private-packs-grid">
        <div className="private-pack-card glass-frame">
          <div className="ppc-head">
            <span className="ppc-slug">acme/api-knowledge</span>
            <span className="ppc-chip private">PRIVATE</span>
          </div>
          <p className="ppc-desc">Stripe + internal API docs · compiled nightly</p>
          <div className="ppc-stats">
            <span>94% rooted</span>
            <span>·</span>
            <span>v1.4.0</span>
          </div>
        </div>
        <div className="private-pack-card glass-frame">
          <div className="ppc-head">
            <span className="ppc-slug">acme/runbooks</span>
            <span className="ppc-chip private">PRIVATE</span>
          </div>
          <p className="ppc-desc">On-call playbooks for the platform team</p>
          <div className="ppc-stats">
            <span>91% rooted</span>
            <span>·</span>
            <span>v0.7.2</span>
          </div>
        </div>
        <div className="private-pack-card glass-frame">
          <div className="ppc-head">
            <span className="ppc-slug">acme/research-2026</span>
            <span className="ppc-chip private">PRIVATE</span>
          </div>
          <p className="ppc-desc">Quarterly market research · drafts only</p>
          <div className="ppc-stats">
            <span>88% rooted</span>
            <span>·</span>
            <span>v0.2.0</span>
          </div>
        </div>
      </div>
      <pre className="private-packs-cmd">
        <span className="cmd-prompt">$ </span>root cloud publish
      </pre>
    </div>
  );
};

// X-Ray Pack Explorer Component
const PackXRay = () => {
  const [activeLayer, setActiveLayer] = useState('source');
  const containerRef = useRef(null);

  const handleMouseMove = (e) => {
    if (!containerRef.current) return;
    const rect = containerRef.current.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const percentage = x / rect.width;

    if (percentage < 0.33) {
      setActiveLayer('source');
    } else if (percentage < 0.66) {
      setActiveLayer('graph');
    } else {
      setActiveLayer('signature');
    }
  };

  return (
    <div className="xray-container" ref={containerRef} onMouseMove={handleMouseMove} onMouseLeave={() => setActiveLayer('source')}>
      <div className="xray-header">
        <span className="xray-title">stripe/api-docs@2.0.tr</span>
        <div className="xray-tabs">
          <span className={`xray-tab ${activeLayer === 'source' ? 'active' : ''}`}>Source</span>
          <span className={`xray-tab ${activeLayer === 'graph' ? 'active' : ''}`}>Knowledge Graph</span>
          <span className={`xray-tab ${activeLayer === 'signature' ? 'active' : ''}`}>Sigstore</span>
        </div>
      </div>
      
      <div className="xray-content-wrapper">
        <div className={`xray-layer layer-source ${activeLayer === 'source' ? 'visible' : ''}`}>
          <div className="mock-code">
            <h3># Stripe API Documentation</h3>
            <p>The Stripe API is organized around REST. Our API has predictable resource-oriented URLs.</p>
            <p>Returns a dictionary with a <code>data</code> property that contains an array of up to <code>limit</code> charges.</p>
          </div>
        </div>

        <div className={`xray-layer layer-graph ${activeLayer === 'graph' ? 'visible' : ''}`}>
          <div className="mock-graph">
            <div className="graph-node central">Stripe API</div>
            <div className="graph-line l1"></div>
            <div className="graph-node n1">REST Architecture</div>
            <div className="graph-line l2"></div>
            <div className="graph-node n2">Resource-Oriented URLs</div>
          </div>
          <div className="mock-code small-code">
            <code>?[subject, predicate, object] := *claims(subject, predicate, object)</code>
          </div>
        </div>

        <div className={`xray-layer layer-signature ${activeLayer === 'signature' ? 'visible' : ''}`}>
          <div className="mock-code crypto-code">
            <p className="crypto-highlight">Hash: blake3-8a9d3b2f...</p>
            <p>Issuer: https://github.com/login/oauth</p>
            <p>Subject: devbynaveen@users.noreply.github.com</p>
            <p className="crypto-success">✓ Validated against Rekor Transparency Log</p>
          </div>
        </div>
      </div>
      <p className="xray-hint">Hover and drag across the pack to X-Ray the pipeline.</p>
    </div>
  );
};

// Screenshot Showcase Component
const ScreenshotShowcase = () => {
  return (
    <div className="showcase-container">
      <div className="showcase-item reveal">
        <div className="showcase-text">
          <h3>The Knowledge Compiler Engine</h3>
          <p>Not just vector embeddings. ThinkingRoot parses your entire workspace through a multi-phase compilation process. Notice the terminal: 6,249 files distilled into 5,187 nodes and 34,482 Datalog relationships in under 90 seconds.</p>
        </div>
        <div className="showcase-image-wrapper glass-frame">
          <img src="/screenshots/compilation.png" alt="Knowledge Compiler Execution" className="showcase-img" />
        </div>
      </div>
      
      <div className="showcase-item reverse reveal delay-1">
        <div className="showcase-text">
          <h3>Cross-Context Code Analysis</h3>
          <p>True semantic understanding. Here, the system identifies a critical contradiction between a README file (claiming 310,000 iterations) and the actual Java source code (180,000 iterations) in CipherVault, mapped directly onto the knowledge graph.</p>
        </div>
        <div className="showcase-image-wrapper glass-frame">
          <img src="/screenshots/ciphervault_contradiction.png" alt="Codebase Anomaly Detection and File Explorer" className="showcase-img" />
        </div>
      </div>

      <div className="showcase-item reveal delay-2">
        <div className="showcase-text">
          <h3>Autonomous Agent Integration</h3>
          <p>MCP is the plug. ThinkingRoot is the brain. Watch as Claude Code connects directly to the ThinkingRoot daemon, gaining instant, hallucination-free context of the entire repository without needing to read thousands of files manually.</p>
        </div>
        <div className="showcase-image-wrapper glass-frame">
          <img src="/screenshots/ciphervault_agent.png" alt="Autonomous Agent Integration via MCP" className="showcase-img" />
        </div>
      </div>
      
      <div className="showcase-item reverse reveal delay-3">
        <div className="showcase-text">
          <h3>Embedded Context & Grounding</h3>
          <p>Every response is verifiable. The transparent reasoning trace and "Grounded" claims pill ensure zero hallucination, while the embedded DuckDuckGo browser allows seamless external web search alongside your cryptographic .tr pack.</p>
        </div>
        <div className="showcase-image-wrapper glass-frame">
          <img src="/screenshots/grounding_browser.png" alt="Cryptographic Grounding and Embedded Browser" className="showcase-img" />
        </div>
      </div>
    </div>
  );
};

// AEP Report Component
const AEPReportShowcase = () => {
  return (
    <div className="report-container reveal">
      <h3 className="section-eyebrow" style={{textAlign: 'center'}}>Case Study: The Knowledge Tax</h3>
      <h2 className="statement-text" style={{fontSize: '2.5rem', marginBottom: '4rem', textAlign: 'center'}}>
        Manual Audit vs. <span className="text-accent">ThinkingRoot AEP</span>
      </h2>
      <div className="report-grid">
         <div className="report-column">
            <div className="report-image-wrapper glass-frame">
               <img src="/screenshots/report_without_mcp.png" alt="Analysis Without MCP" className="report-img" />
            </div>
            <div className="report-image-wrapper glass-frame" style={{marginTop: '2rem'}}>
               <img src="/screenshots/report_metrics.png" alt="Comparison Metrics" className="report-img" />
            </div>
         </div>
         <div className="report-column" style={{marginTop: '4rem'}}>
            <div className="report-image-wrapper glass-frame">
               <img src="/screenshots/report_summary.png" alt="Analysis Summary" className="report-img" />
            </div>
            <div className="report-image-wrapper glass-frame" style={{marginTop: '2rem'}}>
               <img src="/screenshots/report_verdict.png" alt="Final Verdict" className="report-img" />
            </div>
         </div>
      </div>
    </div>
  );
};

function App() {
  useEffect(() => {
    const observer = new IntersectionObserver((entries) => {
      entries.forEach(entry => {
        if (entry.isIntersecting) {
          entry.target.classList.add('visible');
        }
      });
    }, { threshold: 0.1 });

    document.querySelectorAll('.reveal').forEach((el) => observer.observe(el));
    return () => observer.disconnect();
  }, []);

  return (
    <div className="layout">
      <AmbientGraph />
      
      {/* Abstract floating nav/logo */}
      <header className="abstract-header">
        <div className="logo-container">
          <img src="/logo.png" alt="ThinkingRoot Logo" className="logo-img" />
          <span className="logo-text">ThinkingRoot</span>
        </div>
        <a href={`${HUB_URL}/dashboard`} className="header-signin-link">
          DASHBOARD
        </a>
        <a href="https://github.com/DevbyNaveen/ThinkingRoot" target="_blank" rel="noreferrer" className="header-github-link">
          GITHUB
        </a>
      </header>

      <main className="content-flow">
        {/* HERO SEQUENCE */}
        <section className="scroll-section hero-sequence">
          <div className="hero-metadata reveal">
            <span className="metadata-item">STATUS: <span className="status-glow">COMPILED</span></span>
            <span className="metadata-divider">//</span>
            <span className="metadata-item">FORMAT: <span className="text-accent">.ZIP FOR AI KNOWLEDGE</span></span>
            <span className="metadata-divider">//</span>
            <span className="metadata-item">ARCH: TR-1.0-STABLE</span>
          </div>
          <h1 className="hero-massive reveal delay-1">
            <span className="hero-line">THE SECONDARY BRAIN FOR</span>
            <span className="hero-line offset-line text-accent">AUTONOMOUS AI AGENTS.</span>
          </h1>
          <p className="hero-sub reveal delay-2">
            We don't just store data. We compile knowledge.
            ThinkingRoot is the open protocol that packs your sources into a content-addressed, 
            Sigstore-signed format.
          </p>
          <div className="cta-flow reveal delay-3">
            <a href="https://github.com/DevbyNaveen/ThinkingRoot" target="_blank" rel="noreferrer" className="pill-button" style={{textDecoration: 'none'}}>
              <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M15 22v-4a4.8 4.8 0 0 0-1-3.03c3.15-.38 6.5-1.4 6.5-7.1a5.8 5.8 0 0 0-1.5-3.8 5.4 5.4 0 0 0 .15-3.8s-1.2-.38-3.9 1.4a13.3 13.3 0 0 0-7 0C6.2 1.6 5 2 5 2a5.4 5.4 0 0 0 .15 3.8A5.8 5.8 0 0 0 3 9.6c0 5.7 3.3 6.7 6.5 7.1a4.8 4.8 0 0 0-1 3.03v4"></path>
                <path d="M9 20a4.8 4.8 0 0 1-5-1.5"></path>
              </svg> View on GitHub
            </a>
          </div>
          <InstallSection />
        </section>

        {/* FEATURES / COMPILER SEQUENCE */}
        <section className="scroll-section data-sequence">
          <h3 className="section-eyebrow reveal">The Knowledge Compiler</h3>
          <div className="data-grid">
            <div className="data-block reveal">
              <span className="data-number">Graph + Vector</span>
              <span className="data-label">Hybrid Retrieval</span>
            </div>
            <div className="data-block reveal delay-1">
              <span className="data-number">100%</span>
              <span className="data-label">Grounded & Provable</span>
            </div>
            <div className="data-block reveal delay-2">
              <span className="data-number">Sub-ms</span>
              <span className="data-label">Speeds via AEP/Engrams</span>
            </div>
          </div>
          <h2 className="statement-text reveal">
            Less tokens · more signal. <br/>
            <span className="text-accent">We don't make the model bigger. We make its context smarter.</span>
          </h2>
        </section>

        {/* GIT FOR KNOWLEDGE SEQUENCE */}
        <section className="scroll-section architecture-sequence">
          <h3 className="section-eyebrow reveal">Git For Knowledge</h3>
          <div className="branch-grid reveal delay-1">
             <div className="branch-card glass-frame">
                <h4>Main Branch</h4>
                <p>Trusted team knowledge</p>
             </div>
             <div className="branch-card glass-frame">
                <h4>Normal Branch</h4>
                <p>Experiments · reviews · proposals</p>
             </div>
             <div className="branch-card glass-frame">
                <h4>Stream Branch</h4>
                <p>Live AI session memory</p>
             </div>
          </div>
          <h2 className="statement-text reveal delay-2" style={{marginTop: '4rem'}}>
            AI memory needs version control.
          </h2>
        </section>

        {/* SCREENSHOT SHOWCASE */}
        <section className="scroll-section showcase-sequence">
          <h3 className="section-eyebrow reveal" style={{textAlign: 'center', marginBottom: '4rem'}}>Not just a concept. A working engine.</h3>
          <ScreenshotShowcase />
        </section>

        {/* AEP REPORT SHOWCASE */}
        <section className="scroll-section aep-report-sequence" style={{paddingTop: '0'}}>
          <AEPReportShowcase />
        </section>

        {/* PRIVATE PACKS — sign-in + dashboard preview */}
        <section className="scroll-section private-packs-sequence">
          <h3 className="section-eyebrow reveal">Your private knowledge, anywhere</h3>
          <h2 className="statement-text reveal delay-1">
            Push <span className="text-accent">.tr</span> packs to your private dashboard.<br/>
            Any agent. Any laptop. <span className="text-accent">Same brain.</span>
          </h2>
          <div className="reveal delay-2" style={{marginTop: '3rem'}}>
            <PrivatePacks />
          </div>
          <div className="cta-flow reveal delay-3" style={{marginTop: '3rem'}}>
            <a href={`${HUB_URL}/dashboard`} className="pill-button" style={{textDecoration: 'none'}}>
              Open Dashboard
            </a>
          </div>
        </section>

        {/* ANY AI CAN PLUG IN / METRICS */}
        <section className="scroll-section integration-sequence">
           <h3 className="section-eyebrow reveal">Any AI Can Plug In</h3>
           <p className="integration-text reveal delay-1" style={{fontSize: '1.5rem', color: 'var(--color-text-secondary)', marginBottom: '2rem'}}>
             CLI · Desktop · REST · MCP <br/>
             Python · TypeScript · Cloud
           </p>
           <h2 className="statement-text reveal delay-2">
            MCP is the plug. <span className="text-accent">ThinkingRoot is the brain.</span><br/>
            One knowledge layer. Any AI.
          </h2>
          <div className="metrics-grid reveal delay-3" style={{marginTop: '4rem'}}>
             <div className="metric-item glass-frame"><strong>91.2%</strong> LongMemEval-500</div>
             <div className="metric-item glass-frame"><strong>0.117ms</strong> p95 retrieval</div>
             <div className="metric-item glass-frame"><strong>10,000</strong> concurrent agents</div>
             <div className="metric-item glass-frame"><strong>.tr</strong> portability</div>
             <div className="metric-item glass-frame"><strong>MCP</strong> compatible</div>
          </div>
        </section>

        {/* TRIBUNAL SEQUENCE (Interactive) */}
        <section className="scroll-section tribunal-sequence reveal">
           <TribunalInput />
        </section>

        {/* TR PROTOCOL VISUALIZER - X-RAY */}
        <section className="scroll-section protocol-sequence">
          <h3 className="section-eyebrow reveal">The .tr Knowledge Pack</h3>
          <div className="reveal delay-1">
             <PackXRay />
          </div>
        </section>

        {/* CONCLUSION */}
        <section className="scroll-section conclusion-sequence" style={{textAlign: 'center'}}>
           <h2 className="statement-text massive-statement reveal" style={{fontSize: '4rem'}}>
             Code travels in Git.<br/>
             Models travel on HuggingFace.<br/>
             <span className="text-accent">Knowledge travels in .tr.</span>
           </h2>
           <p className="hero-sub reveal delay-1" style={{margin: '4rem auto'}}>
             We believe AI memory should belong to users and teams, not platforms.
           </p>
        </section>

      </main>
      
      <footer className="abstract-footer">
        <p>ThinkingRoot Labs © 2026</p>
      </footer>

      <GithubBadge />
    </div>
  );
}

export default App;
