#pragma once

#include "config.h"
#include "wayfire/core.hpp"
#include "wayfire/output.hpp"
#include "wayfire/unstable/translation-node.hpp"
#include "wayfire/util.hpp"
#include "wayfire/view.hpp"
#include "xwayland-view-base.hpp"
#include <wayfire/render-manager.hpp>
#include <wayfire/scene-operations.hpp>
#include <wayfire/window-manager.hpp>
#include "../core/core-impl.hpp"
#include "../core/seat/seat-impl.hpp"
#include "wayfire/unstable/wlr-view-keyboard-interaction.hpp"

#if WF_HAS_XWAYLAND

namespace wf
{
class xwayland_unmanaged_view_node_t : public wf::scene::translation_node_t, public view_node_tag_t
{
  public:
    xwayland_unmanaged_view_node_t(wayfire_view view) : view_node_tag_t(view)
    {
        _view = view->weak_from_this();
        this->kb_interaction = std::make_unique<wlr_view_keyboard_interaction_t>(view);
    }

    wf::keyboard_focus_node_t keyboard_refocus(wf::output_t *output) override
    {
        auto view = _view.lock();
        if (!view || !view->get_keyboard_focus_surface() || !view->is_mapped())
        {
            return wf::keyboard_focus_node_t{};
        }

        if (output != view->get_output())
        {
            return wf::keyboard_focus_node_t{};
        }

        const uint64_t last_ts = wf::get_core().seat->get_last_focus_timestamp();
        const uint64_t our_ts  = keyboard_interaction().last_focus_timestamp;
        auto cur_focus = wf::get_core_impl().seat->priv->keyboard_focus.get();
        bool has_focus = (cur_focus == this) || (our_ts == last_ts);
        if (has_focus)
        {
            return wf::keyboard_focus_node_t{this, focus_importance::REGULAR};
        }

        return wf::keyboard_focus_node_t{};
    }

    keyboard_interaction_t& keyboard_interaction() override
    {
        return *kb_interaction;
    }

    std::string stringify() const override
    {
        if (auto view = _view.lock())
        {
            std::ostringstream out;
            out << view->self();
            return "unmanaged " + out.str() + " " + stringify_flags();
        } else
        {
            return "inert unmanaged " + stringify_flags();
        }
    }

  protected:
    std::weak_ptr<view_interface_t> _view;
    std::unique_ptr<keyboard_interaction_t> kb_interaction;
};
}

class wayfire_unmanaged_xwayland_view : public wayfire_xwayland_view_internal_base
{
  protected:
    wf::wl_listener_wrapper on_set_geometry;

    /**
     * The bounding box of the view the last time it was rendered.
     *
     * This is used to damage the view when it is resized, because when a
     * transformer changes because the view is resized, we can't reliably
     * calculate the old view region to damage.
     */
    wf::geometry_t last_bounding_box{0, 0, 0, 0};

    void handle_client_configure(wlr_xwayland_surface_configure_event *ev) override
    {
        // We accept the client requests without any modification when it comes to unmanaged views.
        wlr_xwayland_surface_configure(xw, ev->x, ev->y, ev->width, ev->height);
        update_geometry_from_xsurface();
    }

    void update_geometry_from_xsurface()
    {
        wf::scene::damage_node(get_root_node(), last_bounding_box);
        wf::point_t new_position = {xw->x, xw->y};

        // Move to the correct output, if the xsurface has changed geometry
        wf::pointf_t midpoint = {xw->x + xw->width / 2.0, xw->y + xw->height / 2.0};
        wf::output_t *wo = wf::get_core().output_layout->get_output_coords_at(midpoint, midpoint);

        if (wo)
        {
            new_position = new_position - wf::origin(wo->get_layout_geometry());
        }

        surface_root_node->set_offset(new_position);
        if (wo != get_output())
        {
            LOGC(XWL, "Transferring xwayland unmanaged surface ", self(),
                " to output ", wo ? wo->to_string() : "null");
            set_output(wo);
            if (wo && (get_current_impl_type() != wf::xw::view_type::DND))
            {
                wf::scene::readd_front(wo->node_for_layer(wf::scene::layer::UNMANAGED), get_root_node());
            }
        }

        LOGC(XWL, "Xwayland unmanaged surface ", self(), " new position ", new_position);
        last_bounding_box = get_bounding_box();
        wf::scene::damage_node(get_root_node(), last_bounding_box);
        wf::scene::update(surface_root_node, wf::scene::update_flag::GEOMETRY);
    }

    std::shared_ptr<wf::xwayland_unmanaged_view_node_t> surface_root_node;

  public:
    wayfire_unmanaged_xwayland_view(wlr_xwayland_surface *xww) : wayfire_xwayland_view_internal_base(xww)
    {
        LOGE("new unmanaged xwayland surface ", xw->title, " class: ", xw->class_t,
            " instance: ", xw->instance);

        role = wf::VIEW_ROLE_UNMANAGED;
        on_set_geometry.set_callback([&] (void*) { update_geometry_from_xsurface(); });
        on_set_geometry.connect(&xw->events.set_geometry);
        _initialize();
    }

    template<class ConcreteUnmanagedView>
    static std::shared_ptr<wayfire_unmanaged_xwayland_view> create(wlr_xwayland_surface *xw)
    {
        auto self = wf::view_interface_t::create<ConcreteUnmanagedView>(xw);
        self->surface_root_node = std::make_shared<wf::xwayland_unmanaged_view_node_t>(self);
        self->set_surface_root_node(self->surface_root_node);
        self->_initialize();
        return self;
    }

    void handle_map_request(wlr_surface *surface) override
    {
        LOGC(XWL, "Mapping unmanaged xwayland surface ", self());
        update_geometry_from_xsurface();
        do_map(surface, true, false);

        /* We update the keyboard focus before emitting the map event, so that
         * plugins can detect that this view can have keyboard focus.
         *
         * Note: only actual override-redirect views should get their focus disabled */
        kb_focus_enabled = (!xw->override_redirect ||
            wlr_xwayland_or_surface_wants_focus(xw));

        wf::scene::readd_front(get_output()->node_for_layer(wf::scene::layer::UNMANAGED), get_root_node());

        const bool wants_focus = (wlr_xwayland_icccm_input_model(xw) != WLR_ICCCM_INPUT_MODEL_NONE);
        if (kb_focus_enabled && wants_focus)
        {
            wf::get_core().default_wm->focus_request(self());
        }

        emit_view_map();
    }

    void handle_unmap_request() override
    {
        LOGC(XWL, "Unmapping unmanaged xwayland surface ", self());
        emit_view_pre_unmap();
        on_surface_commit.disconnect();
        do_unmap();
    }

    void destroy() override
    {
        on_set_geometry.disconnect();
        wayfire_xwayland_view_internal_base::destroy();
    }

    wf::xw::view_type get_current_impl_type() const override
    {
        return wf::xw::view_type::UNMANAGED;
    }
};

class wayfire_dnd_xwayland_view : public wayfire_unmanaged_xwayland_view
{
  public:
    using wayfire_unmanaged_xwayland_view::wayfire_unmanaged_xwayland_view;

    wf::xw::view_type get_current_impl_type() const override
    {
        return wf::xw::view_type::DND;
    }

    ~wayfire_dnd_xwayland_view()
    {
        LOGD("Destroying a Xwayland drag icon");
    }

    void handle_map_request(wlr_surface *surface) override
    {
        LOGD("Mapping a Xwayland drag icon");
        wayfire_unmanaged_xwayland_view::handle_map_request(surface);
        wf::scene::readd_front(wf::get_core().scene(), this->get_root_node());
    }

    void handle_unmap_request() override
    {
        wayfire_unmanaged_xwayland_view::handle_unmap_request();
        wf::scene::remove_child(this->get_root_node());
    }
};

#endif
