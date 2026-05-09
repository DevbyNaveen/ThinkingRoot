import React, { useEffect, useState, useRef } from 'react';
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
        <a href="https://github.com/DevbyNaveen/ThinkingRoot" target="_blank" rel="noreferrer" className="header-github-link">
          GITHUB
        </a>
      </header>

      <main className="content-flow">
        {/* HERO SEQUENCE */}
        <section className="scroll-section hero-sequence">
          <h1 className="hero-massive reveal">
            <span className="hero-line">MCP DEFINES TOOLS.</span>
            <span className="hero-line offset-line text-accent">WE DEFINE KNOWLEDGE.</span>
          </h1>
          <p className="hero-sub reveal delay-2">
            Every AI tool builds its own private brain. When you switch, you lose everything.
            ThinkingRoot is the open protocol that packs your sources into a content-addressed, 
            Sigstore-signed format.
          </p>
          <div className="cta-flow reveal delay-3">
            <button className="pill-button" onClick={() => navigator.clipboard.writeText('cargo install thinkingroot-cli')}>
              <Terminal size={18} /> cargo install thinkingroot-cli
            </button>
          </div>
        </section>

        {/* DATA / MARKET SEQUENCE */}
        <section className="scroll-section data-sequence">
          <div className="data-grid">
            <div className="data-block reveal">
              <span className="data-number">98<span className="data-unit">ms</span></span>
              <span className="data-label">p95 Compile Latency</span>
            </div>
            <div className="data-block reveal delay-1">
              <span className="data-number">22</span>
              <span className="data-label">Rust Crates. Zero Stubs.</span>
            </div>
            <div className="data-block reveal delay-2">
              <span className="data-number">Aug 2</span>
              <span className="data-label">2026 EU AI Act Deadline</span>
            </div>
          </div>
          <h2 className="statement-text reveal">
            Machine-readable provenance is now a legal requirement. 
            <br/><span className="text-accent">We are the compliance substrate.</span>
          </h2>
        </section>

        {/* TRIBUNAL SEQUENCE (Interactive) */}
        <section className="scroll-section tribunal-sequence reveal">
           <TribunalInput />
        </section>

        {/* TR PROTOCOL VISUALIZER - X-RAY */}
        <section className="scroll-section protocol-sequence">
          <h3 className="section-eyebrow reveal">The .tr Knowledge Pack</h3>
          <h2 className="statement-text reveal mb-4">
            We extract truth. <br/>
            <span className="text-accent">Hover to see how.</span>
          </h2>
          <div className="reveal delay-1">
             <PackXRay />
          </div>
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
