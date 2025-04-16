#include "wayfire/geometry.hpp"
#include "wayfire/plugins/common/input-grab.hpp"
#include "wayfire/scene-input.hpp"
#include "wayfire/txn/transaction-manager.hpp"
#include <wayfire/toplevel.hpp>
#include <cmath>
#include <wayfire/per-output-plugin.hpp>
#include <wayfire/output.hpp>
#include <wayfire/view.hpp>
#include <wayfire/core.hpp>
#include <wayfire/workspace-set.hpp>
#include <linux/input.h>
#include <wayfire/signal-definitions.hpp>
#include <wayfire/plugins/wobbly/wobbly-signal.hpp>
#include <wayfire/nonstd/wlroots-full.hpp>
#include <wlr/util/edges.h>

class wayfire_resize : public wf::per_output_plugin_instance_t, public wf::pointer_interaction_t,
    public wf::touch_interaction_t
{
    wf::signal::connection_t<wf::view_resize_request_signal> on_resize_request =
        [=] (wf::view_resize_request_signal *request)
    {
        if (!request->view)
        {
            return;
        }

        auto touch = wf::get_core().get_touch_position(0);
        if (!std::isnan(touch.x) && !std::isnan(touch.y))
        {
            is_using_touch = true;
        } else
        {
            is_using_touch = false;
        }

        was_client_request = true;
        preserve_aspect    = false;
        initiate(request->view, request->edges);
    };

    wf::signal::connection_t<wf::view_disappeared_signal> on_view_disappeared =
        [=] (wf::view_disappeared_signal *ev)
    {
        if (ev->view == view)
        {
            view = nullptr;
            input_pressed(WLR_BUTTON_RELEASED);
        }
    };

    wf::button_callback activate_binding;
    wf::button_callback activate_binding_preserve_aspect;

    wayfire_toplevel_view view;

    bool was_client_request, is_using_touch;
    bool preserve_aspect = false;
    wf::point_t grab_start;
    wf::geometry_t grabbed_geometry;

    uint32_t edges;
    wf::option_wrapper_t<wf::buttonbinding_t> button{"resize/activate"};
    wf::option_wrapper_t<wf::buttonbinding_t> button_preserve_aspect{
        "resize/activate_preserve_aspect"};
    std::unique_ptr<wf::input_grab_t> input_grab;
    wf::plugin_activation_data_t grab_interface = {
        .name = "resize",
        .capabilities = wf::CAPABILITY_GRAB_INPUT | wf::CAPABILITY_MANAGE_DESKTOP,
    };

  public:
    void init() override
    {
        input_grab = std::make_unique<wf::input_grab_t>("resize", output, nullptr, this, this);

        activate_binding = [=] (auto)
        {
            return activate(false);
        };

        activate_binding_preserve_aspect = [=] (auto)
        {
            return activate(true);
        };

        output->add_button(button, &activate_binding);
        output->add_button(button_preserve_aspect, &activate_binding_preserve_aspect);
        grab_interface.cancel = [=] ()
        {
            input_pressed(WLR_BUTTON_RELEASED);
        };

        output->connect(&on_resize_request);
        output->connect(&on_view_disappeared);
    }

    bool activate(bool should_preserve_aspect)
    {
        auto view = toplevel_cast(wf::get_core().get_cursor_focus_view());
        if (view)
        {
            is_using_touch     = false;
            was_client_request = false;
            preserve_aspect    = should_preserve_aspect;
            initiate(view);
        }

        return false;
    }

    void handle_pointer_button(const wlr_pointer_button_event& event) override
    {
        if ((event.state == WL_POINTER_BUTTON_STATE_RELEASED) && was_client_request &&
            (event.button == BTN_LEFT))
        {
            return input_pressed(event.state);
        }

        if ((event.button != wf::buttonbinding_t(button).get_button()) &&
            (event.button != wf::buttonbinding_t(button_preserve_aspect).get_button()))
        {
            return;
        }

        input_pressed(event.state);
    }

    void handle_pointer_motion(wf::pointf_t pointer_position, uint32_t time_ms) override
    {
        input_motion();
    }

    void handle_touch_up(uint32_t time_ms, int finger_id, wf::pointf_t lift_off_position) override
    {
        if (finger_id == 0)
        {
            input_pressed(WLR_BUTTON_RELEASED);
        }
    }

    void handle_touch_motion(uint32_t time_ms, int finger_id, wf::pointf_t position) override
    {
        if (finger_id == 0)
        {
            input_motion();
        }
    }

    /* Returns the currently used input coordinates in global compositor space */
    wf::point_t get_global_input_coords()
    {
        wf::pointf_t input;
        if (is_using_touch)
        {
            input = wf::get_core().get_touch_position(0);
        } else
        {
            input = wf::get_core().get_cursor_position();
        }

        return {(int)input.x, (int)input.y};
    }

    /* Returns the currently used input coordinates in output-local space */
    wf::point_t get_input_coords()
    {
        auto og = output->get_layout_geometry();

        return get_global_input_coords() - wf::point_t{og.x, og.y};
    }

    /* Calculate resize edges, grab starts at (sx, sy), view's geometry is vg */
    uint32_t calculate_edges(wf::geometry_t vg, int sx, int sy)
    {
        int view_x = sx - vg.x;
        int view_y = sy - vg.y;

        uint32_t edges = 0;
        if (view_x < vg.width / 2)
        {
            edges |= WLR_EDGE_LEFT;
        } else
        {
            edges |= WLR_EDGE_RIGHT;
        }

        if (view_y < vg.height / 2)
        {
            edges |= WLR_EDGE_TOP;
        } else
        {
            edges |= WLR_EDGE_BOTTOM;
        }

        return edges;
    }

    bool initiate(wayfire_toplevel_view view, uint32_t forced_edges = 0)
    {
        if (!view || (view->role == wf::VIEW_ROLE_DESKTOP_ENVIRONMENT) ||
            !view->is_mapped() || view->pending_fullscreen())
        {
            return false;
        }

        this->edges = forced_edges ?: calculate_edges(view->get_bounding_box(),
            get_input_coords().x, get_input_coords().y);

        if ((edges == 0) || !(view->get_allowed_actions() & wf::VIEW_ALLOW_RESIZE))
        {
            return false;
        }

        if (!output->activate_plugin(&grab_interface))
        {
            return false;
        }

        input_grab->set_wants_raw_input(true);
        input_grab->grab_input(wf::scene::layer::OVERLAY);

        grab_start = get_input_coords();
        grabbed_geometry = view->get_geometry();
        if (view->pending_tiled_edges())
        {
            view->toplevel()->pending().tiled_edges = 0;
        }

        this->view = view;

        auto og = view->get_bounding_box();
        int anchor_x = og.x;
        int anchor_y = og.y;

        if (edges & WLR_EDGE_LEFT)
        {
            anchor_x += og.width;
        }

        if (edges & WLR_EDGE_TOP)
        {
            anchor_y += og.height;
        }

        start_wobbly(view, anchor_x, anchor_y);
        wf::get_core().set_cursor(wlr_xcursor_get_resize_name((wlr_edges)edges));

        return true;
    }

    void input_pressed(uint32_t state)
    {
        if (state != WLR_BUTTON_RELEASED)
        {
            return;
        }

        input_grab->ungrab_input();
        output->deactivate_plugin(&grab_interface);

        if (view)
        {
            end_wobbly(view);

            wf::view_change_workspace_signal workspace_may_changed;
            workspace_may_changed.view = this->view;
            workspace_may_changed.to   = output->wset()->get_current_workspace();
            workspace_may_changed.old_workspace_valid = false;
            output->emit(&workspace_may_changed);
        }
    }

    // Convert resize edges to gravity
    uint32_t calculate_gravity()
    {
        uint32_t gravity = 0;
        if (edges & WLR_EDGE_LEFT)
        {
            gravity |= WLR_EDGE_RIGHT;
        }

        if (edges & WLR_EDGE_RIGHT)
        {
            gravity |= WLR_EDGE_LEFT;
        }

        if (edges & WLR_EDGE_TOP)
        {
            gravity |= WLR_EDGE_BOTTOM;
        }

        if (edges & WLR_EDGE_BOTTOM)
        {
            gravity |= WLR_EDGE_TOP;
        }

        return gravity;
    }

    wf::dimensions_t calculate_min_size()
    {
        // Min size, if not set to something larger, is 1x1 + decoration size
        wf::dimensions_t min_size = view->toplevel()->get_min_size();
        min_size.width  = std::max(1, min_size.width);
        min_size.height = std::max(1, min_size.height);
        min_size = wf::expand_dimensions_by_margins(min_size,
            view->toplevel()->pending().margins);

        return min_size;
    }

    wf::dimensions_t calculate_max_size(wf::dimensions_t min)
    {
        // Max size is whatever is set by the client, if not set, then it is MAX_INT
        wf::dimensions_t max_size = view->toplevel()->get_max_size();
        if (max_size.width > 0)
        {
            max_size.width += view->toplevel()->pending().margins.left +
                view->toplevel()->pending().margins.right;
        } else
        {
            max_size.width = std::numeric_limits<decltype(max_size.width)>::max();
        }

        if (max_size.height > 0)
        {
            max_size.height += view->toplevel()->pending().margins.top +
                view->toplevel()->pending().margins.bottom;
        } else
        {
            max_size.height = std::numeric_limits<decltype(max_size.height)>::max();
        }

        // Sanitize values in case desired.width/height gets negative for example.
        max_size.width  = std::max(max_size.width, min.width);
        max_size.height = std::max(max_size.height, min.height);

        return max_size;
    }

    bool does_shrink(int dx, int dy)
    {
        if (std::abs(dx) > std::abs(dy))
        {
            return dx < 0;
        } else
        {
            return dy < 0;
        }
    }

    void input_motion()
    {
        auto input = get_input_coords();
        int dx     = input.x - grab_start.x;
        int dy     = input.y - grab_start.y;

        wf::geometry_t desired = grabbed_geometry;
        double ratio = 1.0;
        if (preserve_aspect)
        {
            ratio = (double)desired.width / desired.height;
        }

        if (edges & WLR_EDGE_LEFT)
        {
            desired.x     += dx;
            desired.width -= dx;
        } else if (edges & WLR_EDGE_RIGHT)
        {
            desired.width += dx;
        }

        if (edges & WLR_EDGE_TOP)
        {
            desired.y += dy;
            desired.height -= dy;
        } else if (edges & WLR_EDGE_BOTTOM)
        {
            desired.height += dy;
        }

        auto min_size = calculate_min_size();
        auto max_size = calculate_max_size(min_size);
        auto desired_unconstrained = desired;
        if (preserve_aspect)
        {
            float new_ratio = 1.0 * desired.width / desired.height;
            if ((new_ratio < ratio) ^ does_shrink(desired.width - grabbed_geometry.width,
                desired.height - grabbed_geometry.height))
            {
                // If we do not shrink: the window is taller than it should be, so expand width first.
                // If we do shrink: the window is wider than it should be, so shrink width first.
                desired.width = std::clamp(int(desired.height * ratio),
                    min_size.width, max_size.width);
                desired.height = std::clamp(int(desired.width / ratio),
                    min_size.height, max_size.height);
                // Clamp once more to ensure we fit the min/max sizes.
                desired.width = std::clamp(int(desired.height * ratio),
                    min_size.width, max_size.width);
            } else
            {
                // The window is wider than it should be, so expand height first.
                desired.height = std::clamp(int(desired.width / ratio),
                    min_size.height, max_size.height);
                desired.width = std::clamp(int(desired.height * ratio),
                    min_size.width, max_size.width);
                // Clamp once more to ensure we fit the min/max sizes.
                desired.height = std::clamp(int(desired.width / ratio),
                    min_size.height, max_size.height);
            }
        } else
        {
            desired.width  = std::clamp(desired.width, min_size.width, max_size.width);
            desired.height = std::clamp(desired.height, min_size.height, max_size.height);
        }

        // If we had to change the size due to ratio/min/max constraints, make sure to keep the gravity
        // correct.
        if (edges & WLR_EDGE_LEFT)
        {
            desired.x += desired_unconstrained.width - desired.width;
        }

        if (edges & WLR_EDGE_TOP)
        {
            desired.y += desired_unconstrained.height - desired.height;
        }

        if (wf::dimensions(view->toplevel()->pending().geometry) != wf::dimensions(desired))
        {
            view->toplevel()->pending().gravity  = calculate_gravity();
            view->toplevel()->pending().geometry = desired;
            wf::get_core().tx_manager->schedule_object(view->toplevel());
        }
    }

    void fini() override
    {
        if (input_grab->is_grabbed())
        {
            input_pressed(WLR_BUTTON_RELEASED);
        }

        output->rem_binding(&activate_binding);
        output->rem_binding(&activate_binding_preserve_aspect);
    }
};

DECLARE_WAYFIRE_PLUGIN(wf::per_output_plugin_t<wayfire_resize>);
